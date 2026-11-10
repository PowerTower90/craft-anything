//! The light asset work, done by the Assets role's AI (a local model when one is set up):
//! `asset_describe` writes a short description of what each inventory item shows and is for,
//! looking at its thumbnail or model preview; `project_inventory_sort_with_agent` proposes
//! category terms for the unsorted items and applies them; `ai_backend_status` and
//! `ai_backend_start` look at and start a model server on this computer.
//!
//! The model is asked through the OpenAI chat API (pictures as `image_url` data URLs), one short
//! request at a time with a deadline, two at once.

use crate::config::{AiConfig, BackendEntry, is_local_server};
use crate::{BackendKind, Role, http};
use craft_anything_engine::asset_categories::{self, Category, CategoryRule};
use craft_anything_engine::evidence::{ErrorCode, ToolError};
use craft_anything_engine::inventory::{self, Item, Kind};
use craft_anything_engine::tool::{Ctx, Effects, Progress, Requires, ToolOutput, ToolSpec, Toolset, parse_params, schema_of};
use craft_anything_engine::workspace::Workspace;
use craft_anything_engine::{CancelToken, Session};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Items described at once.
pub const CONCURRENCY: usize = 2;
/// How long one answer may take (a model loading for the first request included).
const ANSWER_TIMEOUT: Duration = Duration::from_secs(180);
/// How long a started server has to answer.
const START_WAIT: Duration = Duration::from_secs(10 * 60);
/// Most items one call describes.
const MAX_ITEMS: usize = 5000;
/// Descriptions are saved this often, so a stopped run keeps what it did.
const SAVE_EVERY: usize = 20;
/// The largest picture sent.
const MAX_PICTURE: u64 = 2 * 1024 * 1024;

pub fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            id: "asset_describe",
            title: "Describe assets with the AI",
            summary: "Write a short plain description of what each inventory item shows and what it's for in the game, by the Assets role's AI (a local model when one is set up; or `backend`): it looks at the item's thumbnail or model preview with its name, kind, category and technical role (taken from the name: colour, normal map, mask, vertex animation data…; never guessed by the model). Sounds and music are described from their name and length. Stored in the inventory (description, described_by), used by rebuilds instead of writing new ones. `ids` or one `category`; items already described are skipped unless `force`. Two at a time, with progress; resumable.",
            beginner: "Writes a short description of each asset, so remaking them follows what they are.",
            toolset: Toolset::Core,
            effects: Effects::WRITES_FILESYSTEM.union(Effects::ACCESSES_NETWORK),
            requires: Requires::Nothing,
            input_schema: schema_of::<DescribeParams>,
            run: asset_describe,
        },
        ToolSpec {
            id: "project_inventory_sort_with_agent",
            title: "Sort the unsorted with the AI",
            summary: "Ask the Assets role's AI (a local model when one is set up; or `backend`) to sort the project's Unsorted assets: it reads the common words of their names (project_inventory_unsorted_terms) with examples and a few thumbnails, proposes category terms, and they're applied as the project's category rules (`apply`, default true). Returns the terms added and the counts per category.",
            beginner: "Lets the AI put the unsorted assets into categories.",
            toolset: Toolset::Core,
            effects: Effects::WRITES_FILESYSTEM.union(Effects::ACCESSES_NETWORK),
            requires: Requires::Nothing,
            input_schema: schema_of::<SortParams>,
            run: project_inventory_sort_with_agent,
        },
        ToolSpec {
            id: "project_inventory_check_grouping",
            title: "Check the grouping with the AI",
            summary: "Ask the Assets role's AI (a local model when one is set up; or `backend`) to review how the project's files are grouped into things (project_inventory_things) in one `category`: the files in no thing, and things of one file, with a few previews. It proposes moves into things and merges, applied as an agent's choices (project_inventory_thing_set, by: agent; the person's own choices are never changed). Returns what it changed (`apply` false: only what it proposes).",
            beginner: "Lets the AI check which files belong together.",
            toolset: Toolset::Core,
            effects: Effects::WRITES_FILESYSTEM.union(Effects::ACCESSES_NETWORK),
            requires: Requires::Nothing,
            input_schema: schema_of::<CheckParams>,
            run: project_inventory_check_grouping,
        },
        ToolSpec {
            id: "ai_backend_status",
            title: "Check the local model",
            summary: "Whether each model server on this computer or its network that's set up in Settings → AI is running (it lists its models), and whether it has a start command; which one does the Assets work.",
            beginner: "Checks whether the AI on this computer is running.",
            toolset: Toolset::Core,
            effects: Effects::ACCESSES_NETWORK,
            requires: Requires::Nothing,
            input_schema: schema_of::<StatusParams>,
            run: ai_backend_status,
        },
        ToolSpec {
            id: "ai_backend_start",
            title: "Start the local model",
            summary: "Start a model server that's set up in Settings → AI with a start command (`name`; default: the Assets role's, else the first one with a start command), detached, and wait until it lists its models (up to 10 minutes while it loads).",
            beginner: "Starts the AI on this computer.",
            toolset: Toolset::Core,
            effects: Effects::LAUNCHES_PROCESS.union(Effects::ACCESSES_NETWORK),
            requires: Requires::Nothing,
            input_schema: schema_of::<StartParams>,
            run: ai_backend_start,
        },
    ]
}

// ---- The model ----

/// One OpenAI-format chat endpoint.
#[derive(Debug, Clone)]
pub struct AssetModel {
    pub name: String,
    pub base: String,
    pub key: Option<String>,
    pub model: String,
}

impl AssetModel {
    /// The endpoint of an API backend (not an agent app, which can't be asked directly).
    pub fn of(e: &BackendEntry) -> Result<AssetModel, ToolError> {
        let c = &e.config;
        let base = match c.kind {
            BackendKind::OpenaiCompatible => c.base_url.clone().unwrap_or_else(|| "http://127.0.0.1:11434/v1".into()),
            BackendKind::Openai => c.base_url.clone().unwrap_or_else(|| crate::openai::OPENAI_BASE.into()),
            BackendKind::Openrouter => c.base_url.clone().unwrap_or_else(|| crate::openai::OPENROUTER_BASE.into()),
            _ => {
                return Err(ToolError::new(ErrorCode::BadSettings, format!("{} can't be asked directly for asset work.", c.name)).with_remediation(
                    "Set up a local model (llama.cpp, Ollama, LM Studio) and choose it for Assets in Settings → AI; the builder writes its own descriptions otherwise.",
                ));
            }
        };
        let key = if is_local_server(c) {
            crate::providers::env_key(&base)
        } else {
            crate::KeyStore::system().get(&c.name).or_else(|| crate::providers::env_key(&base))
        };
        Ok(AssetModel { name: c.name.clone(), base: base.trim_end_matches('/').to_string(), key, model: c.model.clone() })
    }

    fn headers(&self) -> Vec<(&'static str, String)> {
        self.key.iter().map(|k| ("authorization", format!("Bearer {k}"))).collect()
    }

    /// Whether it answers, and the models it lists.
    pub fn models(&self, timeout: Duration) -> Result<Vec<String>, ToolError> {
        let agent = http::quick_agent(timeout);
        let v = http::get_json(&agent, &self.name, &format!("{}/models", self.base), &self.headers())?;
        Ok(v.get("data")
            .or_else(|| v.get("models"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|m| m["id"].as_str().map(str::to_string))
            .collect())
    }

    /// One answer to `system` and `user` (text, and a PNG or JPEG when given).
    pub fn ask(&self, system: &str, user: &str, picture: Option<&Path>, max_tokens: u32) -> Result<String, ToolError> {
        use base64::Engine as _;
        let mut parts = vec![json!({"type": "text", "text": user})];
        if let Some(p) = picture {
            let media = if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("jpg") || e.eq_ignore_ascii_case("jpeg")) { "image/jpeg" } else { "image/png" };
            let bytes = std::fs::read(p).map_err(|e| ToolError::io(p.display(), e))?;
            parts.push(
                json!({"type": "image_url", "image_url": {"url": format!("data:{media};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes))}}),
            );
        }
        let body = json!({
            "model": self.model, "stream": false, "temperature": 0.2, "max_tokens": max_tokens,
            "messages": [{"role": "system", "content": system}, {"role": "user", "content": parts}],
        });
        let agent = http::agent(Duration::from_secs(10), ANSWER_TIMEOUT, ANSWER_TIMEOUT);
        let mut r = http::post_json(&agent, &self.name, &format!("{}/chat/completions", self.base), &self.headers(), &body)?;
        let mut text = String::new();
        std::io::Read::read_to_string(&mut r, &mut text)
            .map_err(|e| ToolError::new(ErrorCode::GarbledOutput, format!("{} stopped answering: {e}", self.name)))?;
        let v: Value =
            serde_json::from_str(&text).map_err(|e| ToolError::new(ErrorCode::GarbledOutput, format!("{} sent JSON we couldn't read: {e}", self.name)))?;
        let answer = v["choices"][0]["message"]["content"].as_str().unwrap_or_default();
        // Thinking models may put their reasoning first.
        let answer = answer.rsplit("</think>").next().unwrap_or(answer).trim();
        if answer.is_empty() {
            return Err(ToolError::new(ErrorCode::GarbledOutput, format!("{} gave an empty answer.", self.name)));
        }
        Ok(answer.to_string())
    }
}

/// The backend that does the asset work: `name`, else the Assets role's.
pub fn assets_backend(session: &Session, name: Option<&str>) -> Result<BackendEntry, ToolError> {
    let cfg = AiConfig::from_table(&session.settings.ai)?;
    let entry = match name {
        Some(n) => cfg.get(n).cloned().ok_or_else(|| ToolError::invalid(format!("No AI backend named {n:?}.")))?,
        None => cfg
            .backend_for(Role::Assets)
            .cloned()
            .ok_or_else(|| ToolError::unavailable("No AI is set up yet.").with_remediation("Add one in Settings → AI."))?,
    };
    if !entry.config.enabled {
        return Err(ToolError::new(ErrorCode::BadSettings, format!("{} is turned off.", entry.config.name)).with_remediation("Turn it on in Settings → AI."));
    }
    Ok(entry)
}

// ---- Roles from names ----

/// What a file is for, technically, when its name says: never left to the model to guess.
pub fn role_from_name(name: &str, kind: Kind) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    let words = asset_categories::words(&lower);
    let has = |w: &str| words.iter().any(|x| x == w);
    let ends = |w: &str| words.last().is_some_and(|x| x == w);
    match kind {
        Kind::Texture | Kind::Sprite => {
            if lower.starts_with("armature_") || has("vat") || has("vertexanim") {
                Some("vertex animation data (positions baked into a texture), not a picture")
            } else if has("normal") || has("normals") || has("nrm") || has("nor") || ends("n") {
                Some("normal map")
            } else if has("mask") || has("masks") {
                Some("mask (channels that say where effects apply)")
            } else if has("alpha") || has("opacity") {
                Some("alpha mask")
            } else if has("emission") || has("emissive") || has("glow") {
                Some("emission map")
            } else if has("rough") || has("roughness") || has("metal") || has("metallic") || has("ao") || has("occlusion") || has("orm") {
                Some("surface map (roughness, metal or shadowing)")
            } else if has("base") || has("albedo") || has("diffuse") || has("color") || has("colour") || has("col") || has("basecolor") {
                Some("colour texture")
            } else if has("icon") || has("ui") || has("hud") || has("button") {
                Some("icon or interface art")
            } else {
                None
            }
        }
        Kind::Model => Some("3D model"),
        Kind::Sound => Some("sound"),
        Kind::Music => Some("music"),
        Kind::Font => Some("font"),
        _ => None,
    }
}

// ---- asset_describe ----

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DescribeParams {
    /// The project's folder.
    pub project: PathBuf,
    /// These items (ids from project_inventory_list).
    #[serde(default)]
    pub ids: Vec<String>,
    /// Or the items of one category.
    #[serde(default)]
    pub category: Option<Category>,
    /// Describe again what's already described.
    #[serde(default)]
    pub force: bool,
    /// At most this many (default 500).
    #[serde(default)]
    pub limit: Option<usize>,
    /// The AI backend to use (default: the Assets role's).
    #[serde(default)]
    pub backend: Option<String>,
}

pub const DESCRIBE_SYSTEM: &str = "You help remake a game's assets with new, original art. For each asset you get its name, kind, category and technical role, and usually a picture of it. Say in one or two plain sentences WHAT it shows (the subject, shape, colours, mood) and what it's for in the game. Don't name the original game, studio or characters' trademarked names; don't describe text or logos letter by letter. The technical role is given: don't guess another one. Don't mention polygons, meshes, animation, game engines or file formats. Answer with the description only.";

/// The question for one item.
pub fn describe_prompt(item: &Item, role: Option<&str>, has_picture: bool) -> String {
    let mut q = format!("Name: {}\nKind: {}\nCategory: {}", item.name, item.kind.id(), item.category.title());
    if let Some(r) = role {
        q.push_str(&format!("\nTechnical role (from its name): {r}"));
    }
    if let (Some(w), Some(h)) = (item.width, item.height) {
        q.push_str(&format!("\nSize: {w}×{h} pixels"));
    }
    if let Some(d) = item.duration_secs {
        q.push_str(&format!("\nLength: {d:.1} seconds"));
    }
    if let Some(d) = item.detail.as_deref() {
        q.push_str(&format!("\nDetail: {d}"));
    }
    if has_picture && item.kind == Kind::Model {
        q.push_str("\nThe picture is a plain grey preview of the 3D model, without its textures: describe its shape and what it is, not the grey colour or the preview.");
    }
    q.push_str(if has_picture {
        "\nDescribe what the picture shows and what it's for."
    } else {
        "\nThere's no picture: describe it from its name and the rest, briefly, and say it's from the name."
    });
    q
}

fn open(project: &Path) -> Result<Workspace, ToolError> {
    Workspace::open(project).map_err(|e| {
        ToolError::new(ErrorCode::NotFound, format!("{} isn't a project folder: {}", project.display(), e.message))
            .with_remediation("Pass a folder from project_list (it holds craft.json).")
    })
}

/// Save these descriptions into the inventory as it is now.
fn store(root: &Path, done: &[(String, String, Option<&'static str>)], by: &str) -> Result<(), ToolError> {
    if done.is_empty() {
        return Ok(());
    }
    let mut inv = inventory::load(root)?;
    if inv.is_newer() {
        return Ok(());
    }
    // The role is the rules' (asset_things); the description is the model's.
    for (id, text, _) in done {
        if let Some(i) = inv.items.iter_mut().find(|i| &i.id == id) {
            i.description = Some(text.clone());
            i.described_by = Some(by.to_string());
            if i.role.is_none() {
                i.role = Some(craft_anything_engine::asset_things::role_of(i));
            }
        }
    }
    inventory::save(root, &inv)
}

fn asset_describe(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let p: DescribeParams = parse_params(params)?;
    let ws = open(&p.project)?;
    let entry = assets_backend(ctx.session, p.backend.as_deref())?;
    let model = AssetModel::of(&entry)?;
    let clean = ctx.session.clean_room();
    let inv = inventory::load(&ws.root)?;
    let todo: Vec<Item> = inv
        .items
        .iter()
        .filter(|i| (p.ids.is_empty() || p.ids.contains(&i.id)) && p.category.is_none_or(|c| i.category == c))
        .filter(|i| p.force || i.description.is_none())
        .filter(|i| !clean || i.source != inventory::Source::Original)
        .take(p.limit.unwrap_or(500).min(MAX_ITEMS))
        .cloned()
        .collect();
    let skipped = inv.items.iter().filter(|i| (p.ids.is_empty() || p.ids.contains(&i.id)) && p.category.is_none_or(|c| i.category == c)).count() - todo.len();
    ctx.after_unlocking_reporting(move |cancel, progress| {
        let t0 = Instant::now();
        let total = todo.len();
        let next = AtomicUsize::new(0);
        let made = craft_anything_platform::sync::Mutex::new(Vec::<(String, String, Option<&'static str>)>::new());
        let failures = craft_anything_platform::sync::Mutex::new(Vec::<Value>::new());
        let unsaved = craft_anything_platform::sync::Mutex::new(Vec::<(String, String, Option<&'static str>)>::new());
        let done = AtomicUsize::new(0);
        let work = |cancel: &CancelToken| {
            while !cancel.is_cancelled() {
                let n = next.fetch_add(1, Ordering::Relaxed);
                let Some(item) = todo.get(n) else { break };
                let role = role_from_name(&item.name, item.kind);
                let picture = item.thumb.as_deref().map(|t| ws.root.join(t)).filter(|f| f.is_file() && std::fs::metadata(f).is_ok_and(|m| m.len() <= MAX_PICTURE));
                let audio = matches!(item.kind, Kind::Sound | Kind::Music);
                let picture = if audio { None } else { picture };
                let asked = model.ask(DESCRIBE_SYSTEM, &describe_prompt(item, role, picture.is_some()), picture.as_deref(), 160);
                match asked {
                    Ok(text) => {
                        let text: String = text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(600).collect();
                        if let Ok(mut m) = made.lock() {
                            m.push((item.id.clone(), text.clone(), role));
                        }
                        if let Ok(mut u) = unsaved.lock() {
                            u.push((item.id.clone(), text, role));
                        }
                    }
                    Err(e) => {
                        if let Ok(mut f) = failures.lock() {
                            f.push(json!({"id": item.id, "name": item.name, "error": e.for_people()}));
                        }
                    }
                }
                let k = done.fetch_add(1, Ordering::Relaxed) + 1;
                progress(Progress { fraction: Some(k as f32 / total.max(1) as f32), message: format!("{k} of {total} described") });
                if k.is_multiple_of(SAVE_EVERY) {
                    let batch = unsaved.lock().map(|mut u| std::mem::take(&mut *u)).unwrap_or_default();
                    let _ = store(&ws.root, &batch, &model.name);
                }
            }
        };
        std::thread::scope(|s| {
            for _ in 0..CONCURRENCY.min(total.max(1)) {
                let c = cancel.clone();
                let w = &work;
                s.spawn(move || w(&c));
            }
        });
        let rest = unsaved.lock().map(|mut u| std::mem::take(&mut *u)).unwrap_or_default();
        store(&ws.root, &rest, &model.name)?;
        let made = made.into_inner().unwrap_or_default();
        let failures = failures.into_inner().unwrap_or_default();
        let names: std::collections::HashMap<&str, &str> = todo.iter().map(|i| (i.id.as_str(), i.name.as_str())).collect();
        Ok(ToolOutput::Value(json!({
            "described": made.len(),
            "failed": failures.len(),
            "skipped_already_described": skipped,
            "backend": model.name,
            "cancelled": cancel.is_cancelled(),
            "seconds": (t0.elapsed().as_secs_f64() * 10.0).round() / 10.0,
            "samples": made.iter().take(12).map(|(id, text, role)| json!({"id": id, "name": names.get(id.as_str()), "role": role, "description": text})).collect::<Vec<_>>(),
            "failures": failures.into_iter().take(20).collect::<Vec<_>>(),
        })))
    });
    Ok(ToolOutput::Value(Value::Null))
}

// ---- Sorting ----

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SortParams {
    /// The project's folder.
    pub project: PathBuf,
    /// Apply the proposed terms (default true); false only returns them.
    #[serde(default = "d_true")]
    pub apply: bool,
    /// The AI backend to use (default: the Assets role's).
    #[serde(default)]
    pub backend: Option<String>,
}

fn d_true() -> bool {
    true
}

pub const SORT_SYSTEM: &str = "You sort a game's asset files into categories by words in their names. Categories: hud (menus, icons, interface), characters (the player and other people), enemies (monsters, creatures to fight), weapons, items (pickups, tools, loot), environment (terrain, buildings, props, sky), effects (particles, explosions, impacts), machines (vehicles, turrets, drones, devices), music, audio (other sounds). Given words with how many files have them and example names, answer with JSON only: {\"rules\": [{\"term\": \"<one of the given words>\", \"category\": \"<category id>\"}]}. Leave out words you aren't sure about.";

/// The terms the model proposed that are among the words asked about, with real categories.
pub fn parse_rules(answer: &str, asked: &[String]) -> Vec<CategoryRule> {
    let start = answer.find('{');
    let end = answer.rfind('}');
    let Some(v) = start.zip(end).filter(|(s, e)| s < e).and_then(|(s, e)| serde_json::from_str::<Value>(&answer[s..=e]).ok()) else { return Vec::new() };
    let mut out: Vec<CategoryRule> = Vec::new();
    for r in v["rules"].as_array().into_iter().flatten() {
        let (Some(term), Some(cat)) = (r["term"].as_str(), r["category"].as_str().and_then(Category::from_id)) else { continue };
        let term = term.trim().to_ascii_lowercase();
        if cat == Category::Unsorted || !asked.contains(&term) || out.iter().any(|o| o.term == term) {
            continue;
        }
        out.push(CategoryRule { term, category: cat });
    }
    out
}

fn project_inventory_sort_with_agent(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let p: SortParams = parse_params(params)?;
    let ws = open(&p.project)?;
    let entry = assets_backend(ctx.session, p.backend.as_deref())?;
    let model = AssetModel::of(&entry)?;
    let clean = ctx.session.clean_room();
    let see = move |i: &Item| !clean || i.source != inventory::Source::Original;
    let inv = inventory::load(&ws.root)?;
    let terms = asset_categories::unsorted_terms(&inv, 60, see);
    if terms.is_empty() {
        return Ok(ToolOutput::Value(json!({"added": [], "note": "Nothing is unsorted."})));
    }
    let thumbs: Vec<PathBuf> = inv
        .items
        .iter()
        .filter(|i| see(i) && i.category == Category::Unsorted)
        .filter_map(|i| i.thumb.as_deref().map(|t| ws.root.join(t)))
        .filter(|f| f.is_file())
        .take(4)
        .collect();
    ctx.after_unlocking_reporting(move |cancel, progress| {
        let t0 = Instant::now();
        let asked: Vec<String> = terms.iter().map(|(t, _, _)| t.clone()).collect();
        let list: Vec<String> = terms.iter().map(|(t, n, ex)| format!("- {t} ({n} files, e.g. {})", ex.join(", "))).collect();
        let question = format!("Words in the unsorted files' names:\n{}", list.join("\n"));
        progress(Progress { fraction: None, message: format!("{} is reading {} words", model.name, asked.len()) });
        // One look at a few of them helps with words that say little.
        let mut seen = Vec::new();
        for t in &thumbs {
            cancel.check()?;
            if let Ok(a) = model.ask("Say in a few words what this game asset shows.", "What is this?", Some(t), 40) {
                seen.push(format!("{}: {a}", t.file_name().map(|f| f.to_string_lossy()).unwrap_or_default()));
            }
        }
        let question = if seen.is_empty() { question } else { format!("{question}\nA few of them look like this:\n{}", seen.join("\n")) };
        cancel.check()?;
        let answer = model.ask(SORT_SYSTEM, &question, None, 1200)?;
        let rules = parse_rules(&answer, &asked);
        let counts = if p.apply && !rules.is_empty() {
            let inv = craft_anything_engine::tools::inventory_tools::categories::change_rules(&ws, &rules, &[])?;
            let mut by: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
            for i in inv.items.iter().filter(|i| see(i)) {
                *by.entry(i.category.id()).or_default() += 1;
            }
            json!(by)
        } else {
            Value::Null
        };
        Ok(ToolOutput::Value(json!({
            "added": rules, "applied": p.apply && !rules.is_empty(), "by_category": counts, "backend": model.name,
            "seconds": (t0.elapsed().as_secs_f64() * 10.0).round() / 10.0,
        })))
    });
    Ok(ToolOutput::Value(Value::Null))
}

// ---- Checking the grouping ----

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckParams {
    /// The project's folder.
    pub project: PathBuf,
    /// The category to check.
    pub category: Category,
    /// Apply what it proposes (default true).
    #[serde(default = "d_true")]
    pub apply: bool,
    /// The AI backend to use (default: the Assets role's).
    #[serde(default)]
    pub backend: Option<String>,
}

pub const CHECK_SYSTEM: &str = "You check how a game's asset files are grouped into things (an enemy, a character, a weapon). You get the things, and files that look misplaced: in no thing, or alone in a thing. Names can have typos and extra words (LOD, Mat, Base, Normal, numbers). Answer with JSON only: {\"moves\": [{\"file\": \"<file id>\", \"thing\": \"<thing key>\"}], \"merges\": [{\"thing\": \"<thing key>\", \"into\": \"<thing key>\"}]}. Use only the given ids and keys. Leave out anything you aren't sure about.";

/// The moves and merges the model proposed, keeping only given files and keys.
/// Pairs of ids: (file, thing) for moves, (thing, into) for merges.
pub type Pairs = Vec<(String, String)>;

pub fn parse_grouping(answer: &str, files: &[String], keys: &[String]) -> (Pairs, Pairs) {
    let v = answer.find('{').zip(answer.rfind('}')).filter(|(a, b)| a < b).and_then(|(a, b)| serde_json::from_str::<Value>(&answer[a..=b]).ok());
    let Some(v) = v else { return (Vec::new(), Vec::new()) };
    let pair = |x: &Value, a: &str, b: &str| Some((x[a].as_str()?.to_string(), x[b].as_str()?.to_string()));
    let moves =
        v["moves"].as_array().into_iter().flatten().filter_map(|m| pair(m, "file", "thing")).filter(|(f, t)| files.contains(f) && keys.contains(t)).collect();
    let merges = v["merges"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| pair(m, "thing", "into"))
        .filter(|(a, b)| a != b && keys.contains(a) && keys.contains(b))
        .collect();
    (moves, merges)
}

fn project_inventory_check_grouping(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    use craft_anything_engine::asset_things;
    let p: CheckParams = parse_params(params)?;
    let ws = open(&p.project)?;
    let entry = assets_backend(ctx.session, p.backend.as_deref())?;
    let model = AssetModel::of(&entry)?;
    let clean = ctx.session.clean_room();
    let inv = inventory::load(&ws.root)?;
    let see = |i: &&Item| (!clean || i.source != inventory::Source::Original) && i.category == p.category;
    let (things, _) = asset_things::things(inv.items.iter().filter(see), Some(p.category));
    let keys: Vec<String> = things.iter().map(|t| t.key.clone()).collect();
    let lonely: Vec<&str> = things.iter().filter(|t| t.files == 1).map(|t| t.key.as_str()).collect();
    let odd: Vec<&Item> = inv
        .items
        .iter()
        .filter(see)
        .filter(|i| {
            i.thing.as_ref().is_none_or(|t| lonely.contains(&t.key.as_str())) && i.thing_by != craft_anything_engine::asset_categories::CategoryBy::User
        })
        .take(150)
        .collect();
    if odd.is_empty() {
        return Ok(ToolOutput::Value(json!({"moved": 0, "merged": 0, "note": "Every file is in a thing with others."})));
    }
    let files: Vec<String> = odd.iter().map(|i| i.id.clone()).collect();
    let list_things: Vec<String> = things.iter().take(200).map(|t| format!("- {} ({}): {} files", t.key, t.name, t.files)).collect();
    let list_files: Vec<String> = odd
        .iter()
        .map(|i| {
            format!(
                "- {} {:?} ({}, {}) now in {}",
                i.id,
                i.name,
                i.kind.id(),
                i.role.map_or("?", |r| r.id()),
                i.thing.as_ref().map_or("no thing", |t| t.key.as_str())
            )
        })
        .collect();
    let question = format!("Things in {}:\n{}\n\nFiles to check:\n{}", p.category.title(), list_things.join("\n"), list_files.join("\n"));
    let pictures: Vec<(String, PathBuf)> =
        odd.iter().filter_map(|i| Some((i.name.clone(), ws.root.join(i.thumb.as_deref()?)))).filter(|(_, f)| f.is_file()).take(4).collect();
    let category = p.category;
    ctx.after_unlocking_reporting(move |cancel, progress| {
        let t0 = Instant::now();
        progress(Progress { fraction: None, message: format!("{} is checking {} files", model.name, files.len()) });
        let mut seen = Vec::new();
        for (name, f) in &pictures {
            cancel.check()?;
            if let Ok(a) = model.ask("Say in a few words what this game asset shows.", &format!("This is {name}. What is it?"), Some(f), 40) {
                seen.push(format!("{name}: {a}"));
            }
        }
        let question = if seen.is_empty() { question } else { format!("{question}\n\nA few of them look like this:\n{}", seen.join("\n")) };
        cancel.check()?;
        let answer = model.ask(CHECK_SYSTEM, &question, None, 2000)?;
        let (moves, merges) = parse_grouping(&answer, &files, &keys);
        let (mut moved, mut merged) = (0usize, 0usize);
        if p.apply {
            let base = |thing: String| craft_anything_engine::tools::inventory_tools::things::ThingSetParams {
                project: ws.root.clone(),
                thing,
                ids: Vec::new(),
                variant: None,
                part: None,
                role: None,
                name: None,
                merge_into: None,
                by: craft_anything_engine::tools::inventory_tools::categories::By::Agent,
            };
            let mut by_thing: std::collections::BTreeMap<String, Vec<String>> = std::collections::BTreeMap::new();
            for (f, t) in &moves {
                by_thing.entry(t.clone()).or_default().push(f.clone());
            }
            for (t, ids) in by_thing {
                let r = craft_anything_engine::tools::inventory_tools::things::set_thing(
                    &ws,
                    &craft_anything_engine::tools::inventory_tools::things::ThingSetParams { ids, ..base(t) },
                    clean,
                )?;
                moved += r["changed"].as_u64().unwrap_or(0) as usize;
            }
            for (a, b) in &merges {
                craft_anything_engine::tools::inventory_tools::things::set_thing(
                    &ws,
                    &craft_anything_engine::tools::inventory_tools::things::ThingSetParams { merge_into: Some(b.clone()), ..base(a.clone()) },
                    clean,
                )?;
                merged += 1;
            }
        }
        let inv = inventory::load(&ws.root)?;
        let counts = asset_things::counts(&inv, |i| !clean || i.source != inventory::Source::Original);
        Ok(ToolOutput::Value(json!({
            "category": category, "checked": files.len(), "moves": moves.iter().map(|(f, t)| json!({"file": f, "thing": t})).collect::<Vec<_>>(),
            "merges": merges.iter().map(|(a, b)| json!({"thing": a, "into": b})).collect::<Vec<_>>(),
            "moved": moved, "merged": merged, "applied": p.apply,
            "things_now": counts.get(&category).map(|c| c.0), "backend": model.name,
            "seconds": (t0.elapsed().as_secs_f64() * 10.0).round() / 10.0,
        })))
    });
    Ok(ToolOutput::Value(Value::Null))
}

// ---- Starting a model server ----

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StatusParams {}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StartParams {
    /// The backend (default: the Assets role's, else the first local one with a start command).
    #[serde(default)]
    pub name: Option<String>,
}

/// The model servers on this computer or its network, and the Assets role's backend.
fn local_backends(session: &Session) -> (Vec<BackendEntry>, Option<String>) {
    let cfg = AiConfig::from_settings(&session.settings);
    let assets = cfg.backend_for(Role::Assets).map(|b| b.config.name.clone());
    (cfg.backends.iter().filter(|b| is_local_server(&b.config)).cloned().collect(), assets)
}

/// A local backend's state: running (with its models) or not, and whether it can be started.
pub fn server_state(e: &BackendEntry) -> Value {
    let start = e.options.start.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let startable = start.is_some_and(|s| Path::new(s).is_file());
    let (running, models, why) = match AssetModel::of(e).and_then(|m| m.models(Duration::from_millis(1500))) {
        Ok(m) => (true, m, None),
        Err(err) => (false, Vec::new(), Some(err.for_people())),
    };
    json!({
        "name": e.config.name, "base_url": e.config.base_url, "model": e.config.model, "enabled": e.config.enabled,
        "state": if running { "running" } else { "not_running" }, "models": models, "start": start, "startable": startable && !running,
        "why": why,
    })
}

fn ai_backend_status(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let _: StatusParams = parse_params(params)?;
    let (list, assets) = local_backends(ctx.session);
    ctx.after_unlocking(move |_| {
        let servers: Vec<Value> = list.iter().map(server_state).collect();
        Ok(ToolOutput::Value(json!({"servers": servers, "assets_backend": assets})))
    });
    Ok(ToolOutput::Value(Value::Null))
}

fn ai_backend_start(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let p: StartParams = parse_params(params)?;
    let (list, assets) = local_backends(ctx.session);
    let has_start = |b: &&BackendEntry| b.options.start.as_deref().is_some_and(|s| !s.trim().is_empty());
    let entry = match &p.name {
        Some(n) => {
            list.iter().find(|b| &b.config.name == n).cloned().ok_or_else(|| ToolError::invalid(format!("{n:?} isn't a model server on this computer.")))?
        }
        None => {
            list.iter().find(|b| Some(&b.config.name) == assets.as_ref() && has_start(b)).or_else(|| list.iter().find(has_start)).cloned().ok_or_else(|| {
                ToolError::new(ErrorCode::EngineMissing, "No model server here has a start command.")
                    .with_remediation("Set one in Settings → AI (Start command).")
            })?
        }
    };
    ctx.after_unlocking_reporting(move |cancel, progress| {
        let model = AssetModel::of(&entry)?;
        let t0 = Instant::now();
        if model.models(Duration::from_millis(1500)).is_ok() {
            return Ok(ToolOutput::Value(json!({"started": false, "already_running": true, "server": server_state(&entry)})));
        }
        let script = entry.options.start.as_deref().map(str::trim).map(PathBuf::from).filter(|s| s.is_file()).ok_or_else(|| {
            ToolError::new(ErrorCode::EngineMissing, format!("{} isn't running and its start command isn't there.", entry.config.name))
                .with_remediation("Set its start command in Settings → AI, or start it yourself.")
        })?;
        progress(Progress { fraction: None, message: format!("Starting {}", entry.config.name) });
        craft_anything_engines::local_gen::launch_script(&script)?;
        loop {
            cancel.check()?;
            if model.models(Duration::from_millis(1500)).is_ok() {
                break;
            }
            if t0.elapsed() >= START_WAIT {
                return Err(ToolError::new(
                    ErrorCode::TimedOut,
                    format!("{} didn't answer within {} minutes of starting.", entry.config.name, START_WAIT.as_secs() / 60),
                )
                .with_remediation("Look at its window or log; loading a large model can take long."));
            }
            progress(Progress { fraction: None, message: format!("Waiting for {} to load ({} s)", entry.config.name, t0.elapsed().as_secs()) });
            std::thread::sleep(Duration::from_secs(2));
        }
        Ok(ToolOutput::Value(json!({"started": true, "seconds": t0.elapsed().as_secs(), "server": server_state(&entry)})))
    });
    Ok(ToolOutput::Value(Value::Null))
}

#[cfg(test)]
mod tests;
