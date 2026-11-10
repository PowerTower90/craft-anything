//! Blender for any agent, headless: `blender_run` (a Python script with helpers, writing only
//! inside the project), `blender_inspect` (a model's numbers), `blender_render` (a model's
//! preview), and `project_inventory_render_models` (previews of the inventory's models, so the
//! gallery shows them). The work is the engines layer's ([`craft_anything_engines::blender`]); it
//! runs after the session is released.

use crate::inventory::{self, Kind};
use crate::session::Session;
use crate::tool::{Ctx, Effects, Progress, Requires, ToolOutput, ToolSpec, Toolset, parse_params, schema_of};
use crate::workspace::{Workspace, really_inside};
use craft_anything_engines::blender::{self, Preview, Setup};
use craft_anything_evidence::{ErrorCode, ToolError};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The longest a script may run.
const MAX_TIMEOUT_SECS: u64 = 60 * 60;
/// Pictures shown back to the model after a run.
const MAX_SHOWN: usize = 2;

pub fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            id: "blender_run",
            title: "Run a Blender script",
            summary: "Run Python (bpy) in headless Blender, with the person's own Blender settings and add-ons. Write only inside the project: the run has its own folder `OUT` (default <project>/blender/<time>, or `out`), the working folder, where relative paths go; an absolute path outside the project is refused. Helpers already defined: clear_scene(); new_material(name, rgba, metallic=0, roughness=0.5); frame_camera(objs=None, view='three_quarter'|'front'|'side'|'top') adds a camera aimed at the meshes and fitted to their bounds, with neutral lights; render_preview(path='preview.png', size=512) renders a PNG with a transparent background (framing first when no camera aims yet); export_glb(path='model.glb', objs=None); export_fbx(path='model.fbx', objs=None); result(**data) prints the RESULT line this tool returns. Returns the exit code, the result, the new files, and the ends of Blender's output; a rendered preview comes back as a picture so you can check it. Never call read_factory_settings.",
            beginner: "Runs a small Blender script to make or change a 3D model in your project.",
            toolset: Toolset::Core,
            effects: Effects::LAUNCHES_PROCESS.union(Effects::WRITES_FILESYSTEM).union(Effects::EXECUTES_FOREIGN_CODE),
            requires: Requires::Nothing,
            input_schema: schema_of::<RunParams>,
            run: blender_run,
        },
        ToolSpec {
            id: "blender_inspect",
            title: "Inspect a model in Blender",
            summary: "Read a model file (GLB, glTF, FBX, OBJ, STL, PLY, .blend) in headless Blender: its objects, meshes, faces, triangles, vertices, materials, textures, armatures and bones, animations, and bounds (min, max, size). `file` is a full path or one relative to `project`.",
            beginner: "Tells you what a 3D model is made of: its size, faces, materials and animations.",
            toolset: Toolset::Core,
            effects: Effects::LAUNCHES_PROCESS.union(Effects::IDEMPOTENT),
            requires: Requires::Nothing,
            input_schema: schema_of::<InspectParams>,
            run: blender_inspect,
        },
        ToolSpec {
            id: "blender_render",
            title: "Render a model preview",
            summary: "Render a preview of a model file in headless Blender (256 px, three-quarter view, neutral light, transparent background) into the project (`out`, default blender/previews/<name>.png). Returns the picture's path and shows it to you.",
            beginner: "Makes a small picture of a 3D model.",
            toolset: Toolset::Core,
            effects: Effects::LAUNCHES_PROCESS.union(Effects::WRITES_FILESYSTEM),
            requires: Requires::Nothing,
            input_schema: schema_of::<RenderParams>,
            run: blender_render,
        },
        ToolSpec {
            id: "project_inventory_render_models",
            title: "Render model previews",
            summary: "Render a preview of every 3D model in the project's inventory that has a model file and no preview yet (or the `ids`, or one `category`; `force` renders again), in headless Blender, a few hundred models per run, into reference/thumbs/<id>.png, so the gallery shows the model instead of a cube. Runs in the background with progress; one model failing doesn't stop the rest.",
            beginner: "Makes pictures of the game's 3D models so you can see them in the gallery.",
            toolset: Toolset::Core,
            effects: Effects::LAUNCHES_PROCESS.union(Effects::WRITES_FILESYSTEM),
            requires: Requires::Nothing,
            input_schema: schema_of::<RenderModelsParams>,
            run: project_inventory_render_models,
        },
    ]
}

/// Where Blender and its helper scripts are, from the session's settings.
pub fn setup_of(session: &Session) -> Setup {
    Setup { blender: session.settings.local_gen.blender.clone(), scripts_dir: session.config_dir().join("blender") }
}

fn open(project: &Path) -> Result<Workspace, ToolError> {
    Workspace::open(project).map_err(|e| {
        ToolError::new(ErrorCode::NotFound, format!("{} isn't a project folder: {}", project.display(), e.message))
            .with_remediation("Pass a folder from project_list (it holds craft.json).")
    })
}

/// A path inside the project from `rel` (relative, no `..`).
fn inside(ws: &Workspace, rel: &str) -> Result<PathBuf, ToolError> {
    let p = Path::new(rel.trim());
    if rel.trim().is_empty() || p.is_absolute() || p.components().any(|c| !matches!(c, std::path::Component::Normal(_) | std::path::Component::CurDir)) {
        return Err(ToolError::invalid(format!("{rel} must be a path inside the project (relative, without ..).")));
    }
    let full = ws.root.join(p);
    if !really_inside(&ws.root, &full) {
        return Err(ToolError::invalid(format!("{rel} leads out of the project.")));
    }
    Ok(full)
}

/// In Clean room, a file of the original (taken out into reference/, or outside any project)
/// isn't opened.
fn clean_room_check(clean: bool, file: &Path, ws: Option<&Workspace>) -> Result<(), ToolError> {
    let original = match ws {
        Some(ws) => !really_inside(&ws.root, file) || file.starts_with(ws.root.join("reference")),
        None => true,
    };
    if clean && original {
        return Err(ToolError::new(ErrorCode::ConsentRequired, "Clean room doesn't open the original's files.")
            .with_remediation("Open a model the project made (inside the project, outside reference/), or turn Deep mode on."));
    }
    Ok(())
}

/// A model file: a full path, or one relative to the project.
fn model_file(file: &str, ws: Option<&Workspace>) -> Result<PathBuf, ToolError> {
    let p = Path::new(file.trim());
    let full = match ws {
        Some(ws) if !p.is_absolute() => inside(ws, file)?,
        _ => p.to_path_buf(),
    };
    if !full.is_absolute() || !full.is_file() {
        return Err(ToolError::new(ErrorCode::NotFound, format!("{file} isn't a file here.")).with_remediation("Pass a full path, or a path inside `project`."));
    }
    if !blender::is_model_file(&full) {
        return Err(ToolError::invalid(format!("{file} isn't a model file Blender reads here ({}).", blender::MODEL_EXTS.join(", "))));
    }
    Ok(full)
}

fn rel(ws: &Workspace, p: &Path) -> String {
    p.strip_prefix(&ws.root).map(|r| r.to_string_lossy().replace('\\', "/")).unwrap_or_else(|_| p.display().to_string())
}

fn stamp() -> String {
    let ms = crate::inbox::now_ms();
    format!("run-{ms}")
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunParams {
    /// The project's folder: the only place the script may write.
    pub project: PathBuf,
    /// The Python script (bpy), using the helpers (OUT, clear_scene, new_material, frame_camera,
    /// render_preview, export_glb, export_fbx, result).
    pub script: String,
    /// The run's folder inside the project (default blender/run-<time>).
    #[serde(default)]
    pub out: Option<String>,
    /// Seconds before it's stopped (default 600, at most 3600).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

fn blender_run(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let p: RunParams = parse_params(params)?;
    if p.script.trim().is_empty() || p.script.len() > 200_000 {
        return Err(ToolError::invalid("Give the script (up to 200 000 characters)."));
    }
    let ws = open(&p.project)?;
    let out = inside(&ws, p.out.as_deref().unwrap_or(&format!("blender/{}", stamp())))?;
    let timeout = Duration::from_secs(p.timeout_secs.unwrap_or(600).clamp(5, MAX_TIMEOUT_SECS));
    let setup = setup_of(ctx.session);
    ctx.after_unlocking_reporting(move |cancel, progress| {
        progress(Progress { fraction: None, message: "Blender is running the script".into() });
        let r = blender::run_agent_script(&setup, &p.script, &ws.root, &out, timeout, &cancel)?;
        let mut pngs: Vec<&PathBuf> = r.new_files.iter().filter(|f| f.extension().is_some_and(|e| e.eq_ignore_ascii_case("png"))).collect();
        pngs.sort_by_key(|f| std::fs::metadata(f).and_then(|m| m.modified()).ok());
        pngs.reverse();
        Ok(ToolOutput::Value(json!({
            "ok": r.run.ok,
            "exit_code": r.run.exit_code,
            "result": r.run.result,
            "out": rel(&ws, &r.out),
            "new_files": r.new_files.iter().map(|f| rel(&ws, f)).collect::<Vec<_>>(),
            "stdout_tail": r.run.stdout_tail,
            "stderr_tail": r.run.stderr_tail,
            "seconds": r.run.seconds,
            "show_images": pngs.iter().take(MAX_SHOWN).map(|f| f.display().to_string()).collect::<Vec<_>>(),
            "note": (!r.run.ok).then_some("The script failed: read stderr_tail (the Python traceback is at its end), fix the script and run it again."),
        })))
    });
    Ok(ToolOutput::Value(Value::Null))
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InspectParams {
    /// The model file: a full path, or a path inside `project`.
    pub file: String,
    /// The project, when `file` is relative to it.
    #[serde(default)]
    pub project: Option<PathBuf>,
}

fn blender_inspect(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let p: InspectParams = parse_params(params)?;
    let ws = p.project.as_deref().map(open).transpose()?;
    let file = model_file(&p.file, ws.as_ref())?;
    clean_room_check(ctx.session.clean_room(), &file, ws.as_ref())?;
    let setup = setup_of(ctx.session);
    ctx.after_unlocking(move |cancel| {
        let mut stats = blender::inspect(&setup, &file, &cancel)?;
        if let Some(o) = stats.as_object_mut() {
            o.insert("file".into(), json!(file.display().to_string()));
        }
        Ok(ToolOutput::Value(stats))
    });
    Ok(ToolOutput::Value(Value::Null))
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RenderParams {
    /// The project: the preview is written inside it.
    pub project: PathBuf,
    /// The model file: a full path, or a path inside the project.
    pub file: String,
    /// Where the PNG goes inside the project (default blender/previews/<name>.png).
    #[serde(default)]
    pub out: Option<String>,
}

fn blender_render(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let p: RenderParams = parse_params(params)?;
    let ws = open(&p.project)?;
    let file = model_file(&p.file, Some(&ws))?;
    clean_room_check(ctx.session.clean_room(), &file, Some(&ws))?;
    let stem = file.file_stem().and_then(|s| s.to_str()).map(craft_anything_engines::local_gen::file_stem).unwrap_or_else(|| "model".into());
    let out = inside(&ws, p.out.as_deref().unwrap_or(&format!("blender/previews/{stem}.png")))?;
    let setup = setup_of(ctx.session);
    ctx.after_unlocking(move |cancel| {
        let r = blender::render_previews(&setup, &[Preview { id: stem.clone(), input: file, output: out.clone() }], &ws.root, &cancel, &|_, _| {})?;
        let r = r.into_iter().next().ok_or_else(|| ToolError::internal("No result."))?;
        if !r.ok {
            return Err(ToolError::new(ErrorCode::ProgramFailed, format!("Blender couldn't render it: {}", r.error.unwrap_or_default())));
        }
        Ok(ToolOutput::Value(json!({"png": rel(&ws, &out), "seconds": r.seconds, "show_images": [out.display().to_string()]})))
    });
    Ok(ToolOutput::Value(Value::Null))
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RenderModelsParams {
    /// The project's folder.
    pub project: PathBuf,
    /// Only these inventory items.
    #[serde(default)]
    pub ids: Vec<String>,
    /// Only this category (hud, characters, enemies, …).
    #[serde(default)]
    pub category: Option<crate::asset_categories::Category>,
    /// Render again even when a preview is there.
    #[serde(default)]
    pub force: bool,
    /// At most this many (default all).
    #[serde(default)]
    pub limit: Option<usize>,
}

/// The models to render: with a model file Blender reads, without a preview (unless `force`).
pub fn models_to_render(ws: &Workspace, inv: &inventory::Inventory, p: &RenderModelsParams) -> Vec<Preview> {
    inv.items
        .iter()
        .filter(|i| i.kind == Kind::Model && (p.ids.is_empty() || p.ids.contains(&i.id)) && p.category.is_none_or(|c| i.category == c))
        .filter(|i| p.force || i.thumb.as_deref().is_none_or(|t| !ws.root.join(t).is_file()))
        .filter_map(|i| {
            let file = ws.root.join(i.path.as_deref()?);
            (file.is_file() && blender::is_model_file(&file)).then(|| Preview {
                id: i.id.clone(),
                input: file,
                output: ws.root.join(format!("reference/thumbs/{}.png", i.id)),
            })
        })
        .take(p.limit.unwrap_or(usize::MAX))
        .collect()
}

fn project_inventory_render_models(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let p: RenderModelsParams = parse_params(params)?;
    let ws = open(&p.project)?;
    let mut inv = inventory::load(&ws.root)?;
    if ctx.session.clean_room() {
        inv.items.retain(|i| i.source != inventory::Source::Original);
    }
    let models = inv.items.iter().filter(|i| i.kind == Kind::Model).count();
    let todo = models_to_render(&ws, &inv, &p);
    if todo.is_empty() {
        let without_file = inv.items.iter().filter(|i| i.kind == Kind::Model && i.path.is_none()).count();
        return Ok(ToolOutput::Value(json!({
            "rendered": 0, "failed": 0, "models": models, "without_file": without_file,
            "note": if without_file > 0 { "Models without a file (not taken out of the original) can't be rendered: scan the original again in Deep mode." } else { "Every model has a preview." },
        })));
    }
    let setup = setup_of(ctx.session);
    ctx.after_unlocking_reporting(move |cancel, progress| {
        let total = todo.len();
        let results = blender::render_previews(&setup, &todo, &ws.root, &cancel, &|done, total| {
            progress(Progress { fraction: Some(done as f32 / total.max(1) as f32), message: format!("{done} of {total} models rendered") })
        })?;
        // Into the inventory as it is now (a scan or a mark may have happened meanwhile).
        let mut inv = inventory::load(&ws.root)?;
        let mut made = 0;
        for r in results.iter().filter(|r| r.ok) {
            if let Some(item) = inv.items.iter_mut().find(|i| i.id == r.id) {
                item.thumb = Some(format!("reference/thumbs/{}.png", r.id));
                made += 1;
            }
        }
        if made > 0 && !inv.is_newer() {
            inventory::save(&ws.root, &inv)?;
        }
        let failures: Vec<Value> = results.iter().filter(|r| !r.ok).take(50).map(|r| json!({"id": r.id, "error": r.error})).collect();
        Ok(ToolOutput::Value(json!({"rendered": made, "failed": total - made.min(total), "failures": failures, "models": models})))
    });
    Ok(ToolOutput::Value(Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::{Item, Source, Status};
    use crate::workspace::TargetRef;
    use crate::{CallOptions, Caller, Engine};

    #[test]
    fn paths_stay_in_the_project_and_only_models_without_previews_are_rendered() {
        let d = tempfile::tempdir().unwrap();
        let ws = Workspace::create(d.path(), "Game", "A game", TargetRef::None).unwrap();
        assert!(inside(&ws, "../x").is_err() && inside(&ws, "/x").is_err() && inside(&ws, "blender/a").is_ok());
        let models = ws.root.join("reference/assets/model");
        std::fs::create_dir_all(&models).unwrap();
        std::fs::write(models.join("a.obj"), "v 0 0 0\n").unwrap();
        std::fs::write(models.join("b.obj"), "v 0 0 0\n").unwrap();
        let mut inv = inventory::Inventory { version: inventory::INVENTORY_VERSION, ..Default::default() };
        for (key, path) in [("a", Some("reference/assets/model/a.obj")), ("b", Some("reference/assets/model/b.obj")), ("c", None)] {
            let mut i = Item::new(Source::Original, key, Kind::Model, key, Status::Original);
            i.path = path.map(str::to_string);
            inv.items.push(i);
        }
        let b = inv.items[1].id.clone();
        std::fs::create_dir_all(ws.root.join("reference/thumbs")).unwrap();
        std::fs::write(ws.root.join(format!("reference/thumbs/{b}.png")), b"png").unwrap();
        inv.items[1].thumb = Some(format!("reference/thumbs/{b}.png"));
        let p = RenderModelsParams { project: ws.root.clone(), ids: vec![], category: None, force: false, limit: None };
        let todo = models_to_render(&ws, &inv, &p);
        assert_eq!(todo.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), [inv.items[0].id.as_str()], "b has a preview, c has no file");
        let forced = models_to_render(&ws, &inv, &RenderModelsParams { force: true, ..p });
        assert_eq!(forced.len(), 2);
        // The tools refuse what isn't a model or isn't in the project.
        let e = Engine::new(crate::testing::session());
        let err = e.call("blender_inspect", json!({"file": "x.png", "project": ws.root}), CallOptions::new(Caller::Mcp)).unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
        let err = e
            .call("blender_render", json!({"file": "reference/assets/model/a.obj", "project": ws.root, "out": "../a.png"}), CallOptions::new(Caller::Mcp))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::BadInput);
        inventory::save(&ws.root, &inv).unwrap();
        let none = e.call("project_inventory_render_models", json!({"project": ws.root, "ids": [b]}), CallOptions::new(Caller::Mcp)).unwrap().to_json();
        assert_eq!(none["result"]["rendered"], 0, "{none}");
    }
}
