//! Generating game assets on this computer's own graphics cards, for free (no keys, no credits):
//! `local_generation_status` and `local_generation_start` for the servers, and
//! `asset_generate_image`, `asset_generate_model` and `asset_generate_batch` to make pictures and
//! 3D models into a project. The work itself is the engines layer's
//! ([`craft_anything_engines::local_gen`]); the generation runs after the session is released, so
//! other tool calls carry on meanwhile, and it reports how far it got.

use crate::inventory::{self, RebuildMark, Source, Status};
use crate::session::Session;
use crate::tool::{Ctx, Effects, Progress, Requires, ToolOutput, ToolSpec, Toolset, parse_params, schema_of};
use crate::workspace::{Workspace, really_inside};
use craft_anything_engines::local_gen::{self, BatchItem, ItemKind, Setup};
use craft_anything_evidence::{ErrorCode, ToolError};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// The folder generated files go in, in the project folder.
pub const GENERATED_DIR: &str = "generated";
/// Most items one batch makes.
const MAX_BATCH: usize = 500;

pub fn specs() -> Vec<ToolSpec> {
    let gen_effects = Effects::LAUNCHES_PROCESS.union(Effects::WRITES_FILESYSTEM).union(Effects::ACCESSES_NETWORK);
    vec![
        ToolSpec {
            id: "local_generation_status",
            title: "Check local generation",
            summary: "Whether this computer can generate assets on its own graphics cards: the picture server and the 3D server (ComfyUI; answering, and which model files they have), background removal (rembg) and Blender, each ready, not running (and whether it can be started), missing a model, or not set up. Set them up with settings_set local_gen.*.",
            beginner: "Checks whether this computer can make pictures and 3D models by itself.",
            toolset: Toolset::Core,
            effects: Effects::ACCESSES_NETWORK,
            requires: Requires::Nothing,
            input_schema: schema_of::<NoParams>,
            run: local_generation_status,
        },
        ToolSpec {
            id: "local_generation_start",
            title: "Start local generation",
            summary: "Start the local generation servers that aren't running (their start commands from local_gen.picture_start and local_gen.shape_start) and wait until they answer (up to 4 minutes). Returns what was started and the status.",
            beginner: "Starts the programs that make pictures and 3D models on this computer.",
            toolset: Toolset::Core,
            effects: Effects::LAUNCHES_PROCESS.union(Effects::ACCESSES_NETWORK),
            requires: Requires::Nothing,
            input_schema: schema_of::<NoParams>,
            run: local_generation_start,
        },
        ToolSpec {
            id: "asset_generate_image",
            title: "Make a picture locally",
            summary: "Make a picture on this computer's graphics card (Z-Image Turbo on the picture server; free): `prompt` says what it shows, `style` how it looks; `transparent` (default true) makes a single subject and takes the background off (rembg). Saved as a PNG at `out` inside the project (default generated/images/<name>.png). Never give it someone else's picture to copy: describe the subject in your own words.",
            beginner: "Makes a picture for your game on this computer, for free.",
            toolset: Toolset::Core,
            effects: gen_effects,
            requires: Requires::Nothing,
            input_schema: schema_of::<ImageParams>,
            run: asset_generate_image,
        },
        ToolSpec {
            id: "asset_generate_model",
            title: "Make a 3D model locally",
            summary: "Make a 3D model on this computer's graphics cards (free): a front-view reference picture from `prompt` and `style` on the picture server, its background taken off, a shape from it on the 3D server (Hunyuan3D 2.1), then finished in Blender: cleaned, decimated to `faces` (default 20000), `height_m` tall (default 1), its pivot at the base, textured from the reference. Writes <out>.glb, <out>.fbx, <out>_preview.png and the reference inside the project (default generated/models/<name>). About a minute per model.",
            beginner: "Makes a 3D model for your game on this computer, for free.",
            toolset: Toolset::Core,
            effects: gen_effects,
            requires: Requires::Nothing,
            input_schema: schema_of::<ModelParams>,
            run: asset_generate_model,
        },
        ToolSpec {
            id: "asset_generate_batch",
            title: "Make many assets locally",
            summary: "Make a list of pictures and 3D models on this computer's graphics cards in one `style` (free): items {id, kind: image|model, description, width/height or height_m, faces, transparent}. Pictures are made one after another on the picture server while the models' shapes are made on the 3D server. Files go to <project>/generated/<job>/<id>.png (or .glb, .fbx, _preview.png) with a manifest at generated/<job>.json; an id that is an inventory item's id marks that item Generated with its file. One item failing doesn't stop the rest; the result lists what failed and why.",
            beginner: "Makes a whole set of assets on this computer, in one style, for free.",
            toolset: Toolset::Core,
            effects: gen_effects,
            requires: Requires::Nothing,
            input_schema: schema_of::<BatchParams>,
            run: asset_generate_batch,
        },
    ]
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NoParams {}

/// Where everything is, from the session's settings.
pub fn setup_of(session: &Session) -> Setup {
    let s = &session.settings.local_gen;
    Setup {
        picture_url: s.picture_url.clone(),
        shape_url: s.shape_url.clone(),
        picture_start: s.picture_start.clone(),
        shape_start: s.shape_start.clone(),
        bg_python: s.bg_python.clone(),
        blender: s.blender.clone(),
        scripts_dir: session.config_dir().join("local_gen"),
        limits: Default::default(),
    }
}

/// What local AI can make here (now, or once its servers are started): for rebuild plans.
pub fn ready(session: &Session) -> crate::asset_rebuild::Ready {
    let st = local_gen::status(&setup_of(session));
    crate::asset_rebuild::Ready { pictures: st.pictures_possible(), models: st.models_possible() }
}

fn status_json(st: &local_gen::Status) -> Value {
    json!({
        "pictures": st.pictures, "shapes": st.shapes, "background_removal": st.background_removal, "blender": st.blender,
        "can_make": {"pictures": st.pictures.ready, "models": st.pictures.ready && st.shapes.ready && st.blender.ready},
        "can_start": {"pictures": st.pictures_possible(), "models": st.models_possible()},
    })
}

fn local_generation_status(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let _: NoParams = parse_params(params)?;
    let setup = setup_of(ctx.session);
    ctx.after_unlocking(move |_| Ok(ToolOutput::Value(status_json(&local_gen::status(&setup)))));
    Ok(ToolOutput::Value(Value::Null))
}

fn local_generation_start(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let _: NoParams = parse_params(params)?;
    let setup = setup_of(ctx.session);
    ctx.after_unlocking_reporting(move |cancel, progress| {
        let started = local_gen::start(&setup, &cancel, &|m| progress(Progress { fraction: None, message: m.to_string() }))?;
        Ok(ToolOutput::Value(json!({"started": started, "status": status_json(&local_gen::status(&setup))})))
    });
    Ok(ToolOutput::Value(Value::Null))
}

fn open(project: &Path) -> Result<Workspace, ToolError> {
    Workspace::open(project).map_err(|e| {
        ToolError::new(ErrorCode::NotFound, format!("{} isn't a project folder: {}", project.display(), e.message))
            .with_remediation("Pass a folder from project_list (it holds craft.json).")
    })
}

/// A path inside the project from `rel` (relative, no `..`), or `default`.
fn out_path(ws: &Workspace, rel: Option<&str>, default: String) -> Result<PathBuf, ToolError> {
    let rel = rel.map(str::trim).filter(|r| !r.is_empty()).map(str::to_string).unwrap_or(default);
    let p = Path::new(&rel);
    if p.is_absolute() || p.components().any(|c| !matches!(c, std::path::Component::Normal(_) | std::path::Component::CurDir)) {
        return Err(ToolError::invalid(format!("{rel} must be a path inside the project (relative, without ..).")));
    }
    let full = ws.root.join(p);
    if !really_inside(&ws.root, &full) {
        return Err(ToolError::invalid(format!("{rel} leads out of the project.")));
    }
    Ok(full)
}

/// A file name from the start of a prompt.
fn name_from(prompt: &str) -> String {
    let words: Vec<String> = prompt.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).take(4).map(str::to_lowercase).collect();
    local_gen::file_stem(&words.join("_"))
}

fn check_prompt(prompt: &str) -> Result<(), ToolError> {
    if prompt.trim().is_empty() || prompt.chars().count() > 2000 {
        return Err(ToolError::invalid("Say what it shows (1 to 2000 characters)."));
    }
    Ok(())
}

fn d_true() -> bool {
    true
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImageParams {
    /// The project's folder.
    pub project: PathBuf,
    /// What the picture shows, in plain words.
    pub prompt: String,
    /// How it looks (e.g. "Clean flat vector", "Pixel art").
    #[serde(default)]
    pub style: String,
    /// Size in pixels (256–2048, default 1024).
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    /// A single subject with its background taken off (default true); false for a texture or a background.
    #[serde(default = "d_true")]
    pub transparent: bool,
    /// The same seed gives the same picture.
    #[serde(default)]
    pub seed: Option<u64>,
    /// Where to save it, inside the project (default generated/images/<name>.png).
    #[serde(default)]
    pub out: Option<String>,
}

fn asset_generate_image(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let p: ImageParams = parse_params(params)?;
    check_prompt(&p.prompt)?;
    let ws = open(&p.project)?;
    let out = out_path(&ws, p.out.as_deref(), format!("{GENERATED_DIR}/images/{}.png", name_from(&p.prompt)))?;
    let setup = setup_of(ctx.session);
    ctx.after_unlocking_reporting(move |cancel, progress| {
        let t0 = Instant::now();
        let say = |m: &str| progress(Progress { fraction: None, message: m.to_string() });
        let prompt = local_gen::picture_prompt(&p.prompt, &p.style, p.transparent);
        let (w, h) = (p.width.unwrap_or(1024), p.height.unwrap_or(1024));
        let seed = p.seed.unwrap_or_else(|| inventory::item_id(Source::Build, &p.prompt).bytes().fold(7u64, |a, b| a.wrapping_mul(31).wrapping_add(u64::from(b))) & 0xffff_ffff);
        let transparent = p.transparent && setup.bg_python.as_deref().is_some_and(Path::is_file);
        let mut files = vec![out.clone()];
        if transparent {
            let raw = out.with_file_name(format!("{}_raw.png", out.file_stem().and_then(|s| s.to_str()).unwrap_or("picture")));
            local_gen::picture(&setup, &prompt, w, h, seed, &raw, &cancel, &say)?;
            say("taking the background off");
            let white = out.with_extension("white.png");
            local_gen::remove_background(&setup, &raw, &out, &white, &cancel)?;
            let _ = std::fs::remove_file(&white);
            files.push(raw);
        } else {
            local_gen::picture(&setup, &prompt, w, h, seed, &out, &cancel, &say)?;
        }
        let bytes = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
        Ok(ToolOutput::Value(json!({
            "file": out.display().to_string(),
            "files": files.iter().map(|f| f.display().to_string()).collect::<Vec<_>>(),
            "bytes": bytes,
            "background_removed": transparent,
            "background_note": (p.transparent && !transparent).then_some("Background removal isn't set up (local_gen.bg_python), so it has a white background."),
            "prompt": prompt,
            "seconds": (t0.elapsed().as_secs_f64() * 10.0).round() / 10.0,
        })))
    });
    Ok(ToolOutput::Value(Value::Null))
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelParams {
    /// The project's folder.
    pub project: PathBuf,
    /// What the model is, in plain words.
    pub prompt: String,
    /// How it looks (e.g. "Low-poly stylised").
    #[serde(default)]
    pub style: String,
    /// Its height in metres (default 1).
    #[serde(default)]
    pub height_m: Option<f32>,
    /// The most faces (500–500000, default 20000).
    #[serde(default)]
    pub faces: Option<u32>,
    /// Where to save it inside the project, without an extension (default generated/models/<name>).
    #[serde(default)]
    pub out: Option<String>,
}

fn asset_generate_model(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let p: ModelParams = parse_params(params)?;
    check_prompt(&p.prompt)?;
    let ws = open(&p.project)?;
    let base = out_path(&ws, p.out.as_deref(), format!("{GENERATED_DIR}/models/{}", name_from(&p.prompt)))?;
    let setup = setup_of(ctx.session);
    let dir = base.parent().map(Path::to_path_buf).unwrap_or_else(|| ws.root.clone());
    let id = base.file_name().and_then(|s| s.to_str()).map(local_gen::file_stem).unwrap_or_else(|| "model".into());
    ctx.after_unlocking_reporting(move |cancel, progress| {
        let item = BatchItem {
            id: id.clone(),
            kind: ItemKind::Model,
            description: p.prompt.clone(),
            width: None,
            height: None,
            transparent: None,
            height_m: p.height_m,
            faces: p.faces,
        };
        let results = local_gen::run_batch(&setup, std::slice::from_ref(&item), &p.style, &dir, &cancel, &|_, _, what| {
            progress(Progress { fraction: None, message: what.to_string() })
        });
        cancel.check()?;
        let r = results.into_iter().next().ok_or_else(|| ToolError::internal("No result."))?;
        if let Some(e) = r.error.filter(|_| !r.ok) {
            return Err(ToolError::new(ErrorCode::ProgramFailed, e));
        }
        let sizes: serde_json::Map<String, Value> =
            r.files.iter().map(|f| (f.display().to_string(), json!(std::fs::metadata(f).map(|m| m.len()).unwrap_or(0)))).collect();
        Ok(ToolOutput::Value(json!({
            "glb": r.files.first().map(|f| f.display().to_string()),
            "fbx": r.files.get(1).map(|f| f.display().to_string()),
            "preview": r.files.get(2).map(|f| f.display().to_string()),
            "reference": r.files.get(3).map(|f| f.display().to_string()),
            "bytes": sizes,
            "stats": r.stats,
            "seconds": (r.seconds * 10.0).round() / 10.0,
        })))
    });
    Ok(ToolOutput::Value(Value::Null))
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BatchParams {
    /// The project's folder.
    pub project: PathBuf,
    /// The job's name: files go to generated/<job>/ (letters, digits, - and _).
    pub job: String,
    /// The style every item is made in.
    #[serde(default)]
    pub style: String,
    /// What to make.
    pub items: Vec<BatchItem>,
}

/// Mark the inventory items the batch made (by id) Generated, with their file.
fn mark_generated(ws: &Workspace, job: &str, results: &[local_gen::ItemResult], clean: bool) -> Result<usize, ToolError> {
    let mut inv = inventory::load(&ws.root)?;
    if inv.is_newer() {
        return Ok(0);
    }
    let now = crate::inbox::now_ms();
    let mut marked = 0;
    for r in results.iter().filter(|r| r.ok) {
        let Some(item) = inv.items.iter_mut().find(|i| i.id == r.id && (!clean || i.source != Source::Original)) else { continue };
        let file = r.files.first().and_then(|f| f.strip_prefix(&ws.root).ok()).map(|f| f.to_string_lossy().replace('\\', "/"));
        item.status = Status::Generated;
        item.marked_ms = Some(now);
        let mut mark =
            item.rebuild.clone().unwrap_or(RebuildMark { method: crate::asset_rebuild::LOCAL_AI.into(), requested_ms: now, job: job.into(), output: None });
        mark.output = file;
        item.rebuild = Some(mark);
        marked += 1;
    }
    if marked > 0 {
        inventory::save(&ws.root, &inv)?;
    }
    Ok(marked)
}

fn asset_generate_batch(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let p: BatchParams = parse_params(params)?;
    if p.items.is_empty() || p.items.len() > MAX_BATCH {
        return Err(ToolError::invalid(format!("items needs 1 to {MAX_BATCH} things to make.")));
    }
    if let Some(bad) = p.items.iter().find(|i| i.description.trim().is_empty() || i.description.chars().count() > 2000 || i.id.trim().is_empty()) {
        return Err(ToolError::invalid(format!("Item {:?} needs an id and a description (up to 2000 characters).", bad.id)));
    }
    let ws = open(&p.project)?;
    let job = local_gen::file_stem(&p.job);
    let dir = out_path(&ws, Some(&format!("{GENERATED_DIR}/{job}")), String::new())?;
    let manifest_path = ws.root.join(GENERATED_DIR).join(format!("{job}.json"));
    let setup = setup_of(ctx.session);
    let clean = ctx.session.clean_room();
    ctx.after_unlocking_reporting(move |cancel, progress| {
        let t0 = Instant::now();
        let results = local_gen::run_batch(&setup, &p.items, &p.style, &dir, &cancel, &|done, total, what| {
            progress(Progress { fraction: Some(done as f32 / total.max(1) as f32), message: format!("{done} of {total} · {what}") })
        });
        let manifest = local_gen::manifest(&job, &p.style, &ws.root, &p.items, &results);
        std::fs::create_dir_all(ws.root.join(GENERATED_DIR)).map_err(|e| ToolError::io(ws.root.join(GENERATED_DIR).display(), e))?;
        let bytes = serde_json::to_vec_pretty(&manifest).map_err(|e| ToolError::internal(e.to_string()))?;
        crate::project::write_atomic(&manifest_path, &bytes)?;
        let marked = mark_generated(&ws, &job, &results, clean)?;
        Ok(ToolOutput::Value(json!({
            "manifest": manifest_path.display().to_string(),
            "made": manifest["made"],
            "failed": manifest["failed"],
            "failures": results.iter().filter(|r| !r.ok).map(|r| json!({"id": r.id, "error": r.error})).collect::<Vec<_>>(),
            "inventory_marked": marked,
            "cancelled": cancel.is_cancelled(),
            "seconds": (t0.elapsed().as_secs_f64() * 10.0).round() / 10.0,
        })))
    });
    Ok(ToolOutput::Value(Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::TargetRef;
    use crate::{CallOptions, Caller, Engine};

    fn engine() -> Engine {
        Engine::new(crate::testing::session())
    }

    #[test]
    fn nothing_running_is_said_plainly_and_paths_stay_in_the_project() {
        let e = engine();
        // The test session points at a port nothing listens on.
        let st = e.call("local_generation_status", json!({}), CallOptions::new(Caller::Mcp)).unwrap().to_json()["result"].clone();
        assert_eq!(st["pictures"]["state"], "not_running", "{st}");
        assert_eq!(st["can_start"]["pictures"], false);
        let err = e.call("local_generation_start", json!({}), CallOptions::new(Caller::Mcp)).unwrap_err();
        assert!(err.message.contains("no start command"), "{}", err.message);
        let d = tempfile::tempdir().unwrap();
        let ws = Workspace::create(d.path(), "Game", "A game", TargetRef::None).unwrap();
        let err =
            e.call("asset_generate_image", json!({"project": ws.root, "prompt": "a gear", "out": "../x.png"}), CallOptions::new(Caller::Mcp)).unwrap_err();
        assert_eq!(err.code, ErrorCode::BadInput);
        let err = e.call("asset_generate_image", json!({"project": ws.root, "prompt": "a gear"}), CallOptions::new(Caller::Mcp)).unwrap_err();
        assert_eq!(err.code, ErrorCode::EngineMissing, "{err:?}");
        let err = e.call("asset_generate_batch", json!({"project": ws.root, "job": "x", "items": []}), CallOptions::new(Caller::Mcp)).unwrap_err();
        assert_eq!(err.code, ErrorCode::BadInput);
        assert_eq!(name_from("A big, red Gear!"), "a_big_red_gear");
    }

    #[test]
    fn a_batch_writes_its_manifest_and_marks_the_items_even_when_one_fails() {
        // Nothing answers: every item fails, the manifest says so, and the call still returns.
        let e = engine();
        let d = tempfile::tempdir().unwrap();
        let ws = Workspace::create(d.path(), "Game", "A game", TargetRef::None).unwrap();
        let items =
            json!([{"id": "abc", "kind": "image", "description": "a shield"}, {"id": "crate", "kind": "model", "description": "a crate", "height_m": 0.8}]);
        let r = e
            .call("asset_generate_batch", json!({"project": ws.root, "job": "hud 1", "style": "Pixel art", "items": items}), CallOptions::new(Caller::Mcp))
            .unwrap()
            .to_json()["result"]
            .clone();
        assert_eq!((r["made"].as_u64(), r["failed"].as_u64()), (Some(0), Some(2)), "{r}");
        let m: Value = serde_json::from_slice(&std::fs::read(ws.root.join("generated/hud_1.json")).unwrap()).unwrap();
        assert_eq!(m["style"], "Pixel art");
        assert!(m["items"][0]["error"].as_str().unwrap_or_default().contains("isn't answering"), "{m}");
        // A made item with an inventory id is marked Generated with its file.
        let mut inv = inventory::Inventory { version: inventory::INVENTORY_VERSION, ..Default::default() };
        let mut item = inventory::Item::new(Source::Build, "app/icon.png", inventory::Kind::Texture, "icon", Status::Placeholder);
        item.id = "abc".into();
        inv.items.push(item);
        inventory::save(&ws.root, &inv).unwrap();
        let made = local_gen::ItemResult {
            id: "abc".into(),
            kind: ItemKind::Image,
            ok: true,
            files: vec![ws.root.join("generated/hud_1/abc.png")],
            error: None,
            seconds: 1.0,
            stats: Value::Null,
        };
        assert_eq!(mark_generated(&ws, "hud_1", &[made], false).unwrap(), 1);
        let saved = inventory::load(&ws.root).unwrap();
        assert_eq!(saved.items[0].status, Status::Generated);
        assert_eq!(saved.items[0].rebuild.as_ref().and_then(|r| r.output.as_deref()), Some("generated/hud_1/abc.png"));
    }
}
