//! Rebuilding a group of assets as cheaply as possible: for each kind of asset in the group, the
//! ways it can be remade ([`METHODS`]), which one each category and kind starts with
//! ([`options`]), what that costs, and starting it: the list goes to
//! `<project>/rebuild/<category>-<time>.json`, the items are marked, and the builder gets a plain
//! brief ([`brief`]).
//!
//! Every method is free: nothing here ever spends money.

use crate::asset_categories::Category;
use crate::inventory::{self, Inventory, Item, Kind, RebuildMark, Status};
use crate::project::write_atomic;
use crate::workspace::Workspace;
use craft_anything_evidence::ToolError;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// The folder rebuild lists go in, in the project folder.
pub const REBUILD_DIR: &str = "rebuild";
/// One way to remake an asset.
#[derive(Debug, Clone, Copy)]
pub struct Method {
    pub id: &'static str,
    pub title: &'static str,
    pub detail: &'static str,
    /// Why it can't be used yet; None when it can.
    pub not_ready: Option<&'static str>,
}

/// Every method, free ones first.
pub const METHODS: &[Method] = &[
    Method {
        id: "engine_ui",
        title: "Built in the engine",
        detail: "Widgets and vector shapes drawn by the game's own code, with an open-licence font.",

        not_ready: None,
    },
    Method {
        id: "engine_materials",
        title: "Engine materials",
        detail: "Engine materials and procedural textures, with Blender where a shape is needed.",

        not_ready: None,
    },
    Method { id: "engine_particles", title: "Engine particles", detail: "The engine's own particle systems.", not_ready: None },
    Method { id: "blender", title: "Blender", detail: "Modelled in Blender from simple shapes.", not_ready: None },
    Method {
        id: LOCAL_AI,
        title: "Local AI on your graphics cards (free)",
        detail: "Made on this computer's graphics cards in the style you choose: pictures, 3D models finished in Blender, sounds, music and voices.",

        // Ready or not depends on this computer: see `Ready`.
        not_ready: None,
    },
    Method { id: "keep", title: "Keep the original for now", detail: "Not rebuilt this time.", not_ready: None },
];

pub fn method(id: &str) -> Option<&'static Method> {
    METHODS.iter().find(|m| m.id == id)
}

/// The style presets offered for local AI (the person can also type their own).
pub const STYLES: &[&str] = &[
    "Clean flat vector",
    "Hand-painted fantasy",
    "Sci-fi neon hologram",
    "Pixel art",
    "Low-poly stylised",
    "Gritty realistic",
    "Cartoon cel-shaded",
    "Match the original's mood",
];

/// The method of the files that come with a thing's new model (maps, masks, animation data).
pub const WITH_MODEL: &str = "with_model";

/// The local AI method's id.
pub const LOCAL_AI: &str = "local_ai";
/// Why local AI can't be used, when it can't.
pub const LOCAL_AI_NOT_READY: &str = "Not running and can't be started: set it up in Settings → Local generation";

/// What this computer can make with local AI now (or once its servers are started).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Ready {
    pub pictures: bool,
    pub models: bool,
    /// Sounds and music (the audio server).
    pub sounds: bool,
}

impl Ready {
    /// Why `id` can't be used for `kind`, if it can't.
    pub fn why_not(self, id: &str, kind: Kind) -> Option<&'static str> {
        if id == LOCAL_AI {
            let ok = match kind {
                Kind::Model => self.models,
                Kind::Sound | Kind::Music => self.sounds,
                _ => self.pictures,
            };
            return (!ok).then_some(LOCAL_AI_NOT_READY);
        }
        method(id).and_then(|m| m.not_ready)
    }
}

/// The methods offered for one kind of asset in one category, the first being the default:
/// [`base_options`], with local AI offered for pictures, models, sounds and music, first where it
/// suits best (the HUD's pictures; the models of characters, enemies, weapons, items and machines;
/// every sound and piece of music) when it's ready.
pub fn options(category: Category, kind: Kind, ready: Ready) -> Vec<&'static str> {
    use Category as C;
    let mut v: Vec<&'static str> = base_options(category, kind).to_vec();
    let picture = matches!(kind, Kind::Texture | Kind::Sprite);
    let model = kind == Kind::Model;
    let audio = matches!(kind, Kind::Sound | Kind::Music);
    if !(picture || model || audio) {
        return v;
    }
    let first = if audio {
        ready.sounds
    } else if picture {
        category == C::Hud && ready.pictures
    } else {
        matches!(category, C::Characters | C::Enemies | C::Weapons | C::Items | C::Machines) && ready.models
    };
    v.insert(if first { 0 } else { 1.min(v.len()) }, LOCAL_AI);
    v
}

/// The methods offered without local AI.
pub fn base_options(category: Category, kind: Kind) -> &'static [&'static str] {
    use Category as C;
    use Kind as K;
    match (category, kind) {
        (_, K::Sound | K::Music) => &["keep"],
        (_, K::Font) => &["engine_ui", "keep"],
        (C::Hud, K::Model) => &["blender", "engine_ui", "keep"],
        (C::Hud, _) => &["engine_ui", "keep"],
        (C::Environment, K::Model) => &["blender", "engine_materials", "keep"],
        (C::Environment, _) => &["engine_materials", "blender", "keep"],
        (C::Effects, _) => &["engine_particles", "keep"],
        (C::Characters | C::Enemies, _) => &["blender", "keep"],
        (C::Items | C::Machines | C::Weapons, K::Texture | K::Sprite | K::Material) => &["engine_materials", "blender", "keep"],
        (C::Items | C::Machines | C::Weapons, _) => &["blender", "keep"],
        (C::Music, _) => &["keep"],
        (C::Unsorted, K::Model) => &["blender", "keep"],
        (C::Unsorted, K::Video) => &["engine_ui", "keep"],
        (C::Unsorted, _) => &["engine_materials", "blender", "keep"],
    }
}

/// One kind of asset in a plan.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub kind: Kind,
    pub count: usize,
    pub method: &'static str,
    pub options: Vec<&'static str>,
    /// Why the chosen method can't be used now, if it can't.
    pub not_ready: Option<&'static str>,
}

impl Row {
    pub fn ready(&self) -> bool {
        self.not_ready.is_none()
    }
}

/// A plan: the items, and for each kind among them the method chosen.
#[derive(Debug, Clone)]
pub struct Plan {
    /// The category the items are in, when they're all in one.
    pub category: Option<Category>,
    pub ids: Vec<String>,
    pub rows: Vec<Row>,
    /// What local AI can make here.
    pub ready: Ready,
    /// The style every local AI generation in this job uses.
    pub style: Option<String>,
    /// Every outfit or version of a thing; else only each thing's main one (the one with the
    /// most files).
    pub outfits: bool,
    /// What the items add up to: things, sounds, files made with a model.
    pub things: ThingSummary,
}

/// A plan's items as things: how many things (with a model), their sounds, the files that come
/// with a model (normal maps, masks, alpha, animation data, effect meshes), and the files of
/// outfits or versions other than each thing's main one.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ThingSummary {
    pub things: usize,
    pub with_model: usize,
    pub sounds: usize,
    pub made_with_model: usize,
    pub other_versions: usize,
    pub noun: &'static str,
}

/// Each thing's main version: the variant with the most files among `items` (none counts too).
pub fn main_versions(items: &[&Item]) -> BTreeMap<String, Option<String>> {
    let mut seen: BTreeMap<String, BTreeMap<Option<String>, usize>> = BTreeMap::new();
    for i in items {
        if let Some(t) = &i.thing {
            *seen.entry(t.key.clone()).or_default().entry(t.variant.clone()).or_default() += 1;
        }
    }
    seen.into_iter()
        .map(|(k, v)| {
            // The base (no variant) when there are only files without one; else the biggest outfit.
            let named: Vec<(&Option<String>, &usize)> = v.iter().filter(|(k, _)| k.is_some()).collect();
            let main = named.iter().max_by_key(|(_, n)| **n).map(|(k, _)| (*k).clone()).unwrap_or(None);
            (k, main)
        })
        .collect()
}

/// Whether an item is left out for being another outfit or version of its thing.
fn other_version(i: &Item, main: &BTreeMap<String, Option<String>>) -> bool {
    match (&i.thing, i.thing.as_ref().and_then(|t| main.get(&t.key))) {
        (Some(t), Some(m)) => t.variant.is_some() && m.is_some() && t.variant != *m,
        _ => false,
    }
}

fn role(i: &Item) -> crate::asset_things::Role {
    i.role.unwrap_or_else(|| crate::asset_things::role_of(i))
}

impl ThingSummary {
    pub fn of(items: &[&Item], category: Option<Category>, outfits: bool) -> ThingSummary {
        let main = main_versions(items);
        let mut keys = std::collections::BTreeSet::new();
        let mut with_model = std::collections::BTreeSet::new();
        let mut s = ThingSummary { noun: category.map_or("things", crate::asset_things::noun), ..Default::default() };
        for i in items {
            let r = role(i);
            if !outfits && other_version(i, &main) {
                s.other_versions += 1;
                continue;
            }
            if r.technical() {
                s.made_with_model += 1;
            } else if r.audio() {
                s.sounds += 1;
            }
            if let Some(t) = i.thing.as_ref().filter(|t| !t.sounds_only) {
                keys.insert(t.key.clone());
                if matches!(r, crate::asset_things::Role::Model | crate::asset_things::Role::ModelPart) {
                    with_model.insert(t.key.clone());
                }
            }
        }
        s.things = keys.len();
        s.with_model = with_model.len();
        s
    }
}

impl Plan {
    /// The plan for `items` (all in `category`, if given), each kind with `chosen` methods
    /// (kind id → method id) or its default. A method the kind doesn't offer is refused.
    pub fn new(items: &[&Item], category: Option<Category>, chosen: &BTreeMap<String, String>, ready: Ready) -> Result<Plan, ToolError> {
        // Files made with a model (normal maps, masks, animation data…) aren't rebuilt on their own.
        let mut by_kind: BTreeMap<Kind, usize> = BTreeMap::new();
        for i in items.iter().filter(|i| !role(i).technical()) {
            *by_kind.entry(i.kind).or_default() += 1;
        }
        let common = category.or_else(|| items.first().map(|i| i.category).filter(|c| items.iter().all(|i| i.category == *c)));
        let group = common.unwrap_or(Category::Unsorted);
        let mut rows = Vec::new();
        for (kind, count) in by_kind {
            let offered = options(group, kind, ready);
            let method = match chosen.get(kind.id()) {
                Some(m) => offered.iter().copied().find(|o| o == m).ok_or_else(|| {
                    ToolError::invalid(format!("{m} isn't a way to rebuild {} here.", kind.id()))
                        .with_remediation(format!("Use one of: {}.", offered.join(", ")))
                })?,
                None => offered.first().copied().unwrap_or("keep"),
            };
            let not_ready = ready.why_not(method, kind);
            rows.push(Row { kind, count, method, options: offered, not_ready });
        }
        let things = ThingSummary::of(items, common, false);
        Ok(Plan { category: common, ids: items.iter().map(|i| i.id.clone()).collect(), rows, ready, style: None, outfits: false, things })
    }

    /// Whether any row is made with local AI (the style matters then).
    pub fn uses_local_ai(&self) -> bool {
        self.rows.iter().any(|r| r.method == LOCAL_AI)
    }

    /// The plan as the tools return it.
    pub fn to_json(&self) -> Value {
        let rows: Vec<Value> = self
            .rows
            .iter()
            .map(|r| {
                let options: Vec<Value> = r
                    .options
                    .iter()
                    .filter_map(|id| method(id))
                    .map(|m| {
                        json!({
                            "id": m.id, "title": m.title, "detail": m.detail,
                            "available": self.ready.why_not(m.id, r.kind).is_none(), "why": self.ready.why_not(m.id, r.kind),
                        })
                    })
                    .collect();
                json!({"kind": r.kind, "count": r.count, "method": r.method, "available": r.ready(), "options": options})
            })
            .collect();
        json!({
            "category": self.category,
            "items": self.ids.len(),
            "rows": rows,
            "total": "Free",
            "uses_local_ai": self.uses_local_ai(),
            "style": self.style,
            "outfits": self.outfits,
            "things": {
                "things": self.things.things, "with_model": self.things.with_model, "sounds": self.things.sounds,
                "made_with_model": self.things.made_with_model, "other_versions": self.things.other_versions, "noun": self.things.noun,
            },
        })
    }
}

/// The items a plan is for: those of `category`, or the ones named by `ids`, that the caller may
/// see (`visible`: Clean room hides the original's).
pub fn select<'a>(inv: &'a Inventory, category: Option<Category>, ids: &[String], visible: impl Fn(&Item) -> bool) -> Result<Vec<&'a Item>, ToolError> {
    if category.is_none() && ids.is_empty() {
        return Err(ToolError::invalid("Say which items: a category, or ids.").with_remediation("project_inventory_list shows the categories and ids."));
    }
    let items: Vec<&Item> =
        inv.items.iter().filter(|i| visible(i)).filter(|i| category.is_none_or(|c| i.category == c) && (ids.is_empty() || ids.contains(&i.id))).collect();
    if items.is_empty() {
        return Err(ToolError::invalid("No items match.").with_remediation("Check the category or ids with project_inventory_list."));
    }
    Ok(items)
}

/// What starting a rebuild did.
#[derive(Debug, Clone)]
pub struct Started {
    /// The list written (`rebuild/<category>-<time>.json`).
    pub list: PathBuf,
    /// How many items were sent to be rebuilt (methods that can start, other than keep).
    pub items: usize,
    /// Rows that didn't start: kept, or their method isn't ready (kind, method, why, count).
    pub skipped: Vec<(Kind, &'static str, String, usize)>,
    /// The brief for the builder (empty when nothing started).
    pub brief: String,
}

/// Start a plan: write its list, mark its items "needs replacing" with how they're to be
/// rebuilt, and save the inventory. Rows whose method isn't ready, and kept ones, are listed as
/// skipped and their items left alone.
pub fn start(ws: &Workspace, inv: &mut Inventory, plan: &Plan, now_ms: u64) -> Result<Started, ToolError> {
    let label = plan.category.map(Category::id).unwrap_or("selected");
    let job = format!("{label}-{now_ms}");
    let rel = format!("{REBUILD_DIR}/{job}.json");
    let path = ws.root.join(&rel);
    let mut skipped = Vec::new();
    let mut going: BTreeMap<Kind, &'static str> = BTreeMap::new();
    for r in &plan.rows {
        match method(r.method) {
            Some(m) if m.id == "keep" => skipped.push((r.kind, m.id, "Kept as it is".to_string(), r.count)),
            Some(m) if !r.ready() => skipped.push((r.kind, m.id, r.not_ready.unwrap_or_default().to_string(), r.count)),
            Some(m) => {
                going.insert(r.kind, m.id);
            }
            None => {}
        }
    }
    // Each thing once: its main version (unless every outfit is wanted); the files that come with
    // a model are listed with it, not made on their own.
    let main = {
        let in_plan: Vec<&Item> = inv.items.iter().filter(|i| plan.ids.contains(&i.id)).collect();
        main_versions(&in_plan)
    };
    let mut listed = Vec::new();
    let mut things: BTreeMap<String, (String, Option<String>, Vec<Value>)> = BTreeMap::new();
    let mut other_versions = 0;
    for item in inv.items.iter_mut().filter(|i| plan.ids.contains(&i.id)) {
        if !plan.outfits && other_version(item, &main) {
            other_versions += 1;
            continue;
        }
        let r = role(item);
        let m: &str = if r.technical() {
            WITH_MODEL
        } else {
            let Some(m) = going.get(&item.kind) else { continue };
            m
        };
        // A map whose thing isn't being remade stays as it is.
        if r.technical() && item.thing.as_ref().is_none_or(|t| !going.contains_key(&Kind::Model) || t.sounds_only) {
            continue;
        }
        item.status = Status::NeedsReplacing;
        item.marked_ms = Some(now_ms);
        item.rebuild = Some(RebuildMark { method: m.to_string(), requested_ms: now_ms, job: job.clone(), output: None });
        let entry = json!({
            "id": item.id, "name": item.name, "kind": item.kind, "category": item.category,
            "path": item.path, "origin": item.origin, "method": m,
            "description": item.description, "role": r, "thing": item.thing.as_ref().map(|t| t.key.clone()),
            "variant": item.thing.as_ref().and_then(|t| t.variant.clone()), "part": item.thing.as_ref().and_then(|t| t.part.clone()),
        });
        if let Some(t) = item.thing.as_ref() {
            let slot = things.entry(t.key.clone()).or_insert_with(|| (t.name.clone(), main.get(&t.key).cloned().flatten(), Vec::new()));
            slot.2.push(json!({"id": item.id, "role": r, "method": m}));
        }
        listed.push(entry);
    }
    if other_versions > 0 {
        skipped.push((
            Kind::Other,
            "keep",
            format!("{other_versions} files of other outfits or versions (only each thing's main one is remade)"),
            other_versions,
        ));
    }
    let things_list: Vec<Value> =
        things.into_iter().map(|(key, (name, variant, files))| json!({"key": key, "name": name, "version": variant, "files": files})).collect();
    if listed.is_empty() {
        return Ok(Started { list: path, items: 0, skipped, brief: String::new() });
    }
    let doc = json!({
        "version": 1,
        "category": plan.category,
        "style": plan.style,
        "created_ms": now_ms,
        "project": ws.root.display().to_string(),
        "methods": going.iter().map(|(k, m)| (k.id().to_string(), json!(m))).collect::<serde_json::Map<String, Value>>(),
        "items": listed,
        "things": things_list,
        "skipped": skipped.iter().map(|(k, m, why, n)| json!({"kind": k, "method": m, "why": why, "count": n})).collect::<Vec<_>>(),
    });
    std::fs::create_dir_all(ws.root.join(REBUILD_DIR)).map_err(|e| ToolError::io(ws.root.join(REBUILD_DIR).display(), e))?;
    let bytes = serde_json::to_vec_pretty(&doc).map_err(|e| ToolError::internal(e.to_string()))?;
    write_atomic(&path, &bytes)?;
    // The project's style for next time.
    if let Some(style) = plan.style.as_ref().filter(|s| !s.trim().is_empty()) {
        inv.last_style = Some(style.trim().to_string());
    }
    inventory::save(&ws.root, inv)?;
    let count = listed.len();
    let style = plan.style.as_deref().filter(|s| !s.trim().is_empty());
    let mut text = brief(plan.category, count, &going, &rel, &job, style);
    if !things_list.is_empty() {
        text.push_str(&format!(
            " Work per thing: the list's `things` ({} of them) groups the files of each one (an enemy, a character with its outfits, a voice group). \
             Make one new model per thing (its parts together for a character), from its description; its colour textures, normal maps, masks, alpha \
             and animation data come with the new model (method {WITH_MODEL}): never generate those files on their own. Then its sounds or voice lines, \
             and replace the thing's files in the game together.",
            things_list.len()
        ));
    }
    Ok(Started { brief: text, list: path, items: count, skipped })
}

/// The builder's brief for a started rebuild. Fixed English: it's for the agent.
pub fn brief(category: Option<Category>, count: usize, methods: &BTreeMap<Kind, &'static str>, list: &str, job: &str, style: Option<&str>) -> String {
    let what = category.map_or_else(|| "the selected".to_string(), |c| format!("the {}", c.title()));
    let how: Vec<String> = methods.iter().map(|(k, m)| format!("{} → {}", k.id(), method(m).map_or(*m, |x| x.title))).collect();
    let mut text = format!(
        "Rebuild {what} assets: {count} items. Methods: {}. The full list, with each item's method, is in {list} (in the project folder). \
         Use free methods only: never use paid services or spend money. Keep the game working while you replace them.",
        how.join("; ")
    );
    if methods.values().any(|m| *m == LOCAL_AI) {
        let style = style.unwrap_or("match the original's mood");
        text.push_str(&format!(
            " For the items made with local AI: look at each original (its thumbnail or path) only to understand WHAT it shows and what it's for in the game, \
             then write a short plain description of the subject in your own words (no copying of logos, text or exact designs). \
             Where the list has a description for an item (written beforehand by asset_describe), use it as that item's description as it is; \
             write your own only for the rest. Call asset_generate_batch once with all the descriptions (project, job \"{job}\", style \"{style}\"; kind image for pictures, \
             kind model with height_m for 3D models; each item's id from the list as its id). Never feed the original file to a generator as an input picture. \
             Then put the generated files into the game (import them, replace the references, keep sizes and aspect ratios; HUD icons keep their \
             transparent background) and rebuild."
        ));
        let audio = methods.iter().any(|(k, m)| *m == LOCAL_AI && matches!(k, Kind::Sound | Kind::Music));
        if audio {
            text.push_str(
                " For sounds, music and voices: listen to/inspect the original only to understand what it is (what makes the sound, how long, the mood; \
                 for speech, transcribe the words). Describe it in your own words; for voices, write one voice description per character and reuse it \
                 for all their lines. Never feed the original audio into a generator. Keep lengths close to the originals, then wire the new files into \
                 the game. In the same asset_generate_batch: kind sound with seconds (the original's duration_secs) and sound sfx, ambience or ui; \
                 kind music with seconds; kind voice for a character's spoken line, with character, voice_description and text (or transcribe_from, \
                 the original's file, transcribed locally for its words only). Decide per item whether it's speech (voice) or a sound. \
                 asset_decode_sound turns a sound in the game's own format (an .fsb bank) into a WAV to listen to and measure.",
            );
        }
    }
    text.push_str(" Mark each item Replaced with project_inventory_mark (status replaced, its id) when its replacement is in the build.");
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::Source;

    fn item(id: &str, kind: Kind, category: Category) -> Item {
        let mut i = Item::new(Source::Original, id, kind, id, Status::Original);
        i.id = id.into();
        i.category = category;
        i
    }

    #[test]
    fn local_ai_is_the_default_where_it_suits_when_ready_and_never_starts_when_not() {
        let ready = Ready { pictures: true, models: true, sounds: false };
        assert_eq!(options(Category::Hud, Kind::Texture, ready), ["local_ai", "engine_ui", "keep"]);
        assert_eq!(options(Category::Hud, Kind::Texture, Ready::default()), ["engine_ui", "local_ai", "keep"]);
        assert_eq!(options(Category::Enemies, Kind::Model, ready)[0], "local_ai");
        assert_eq!(options(Category::Environment, Kind::Texture, ready)[0], "engine_materials", "offered, not the default");
        assert_eq!(options(Category::Hud, Kind::Sound, ready), ["keep", "local_ai"], "sounds wait for the audio server");
        let all = Ready { pictures: true, models: true, sounds: true };
        assert_eq!(options(Category::Characters, Kind::Sound, all), ["local_ai", "keep"], "local AI makes sounds when it can");
        assert_eq!(options(Category::Music, Kind::Music, all), ["local_ai", "keep"]);
        let hud = [item("a", Kind::Texture, Category::Hud), item("m", Kind::Model, Category::Enemies)];
        let refs: Vec<&Item> = hud.iter().collect();
        let mut chosen = BTreeMap::new();
        chosen.insert("model".to_string(), "local_ai".to_string());
        let plan = Plan::new(&refs, None, &chosen, Ready { pictures: true, models: false, sounds: false }).unwrap();
        assert_eq!(plan.rows[1].not_ready, Some(LOCAL_AI_NOT_READY), "models need the 3D server and Blender");
        assert!(plan.uses_local_ai());
        assert_eq!(plan.to_json()["total"], "Free");
        let methods = [(Kind::Texture, LOCAL_AI)].into_iter().collect();
        let text = brief(Some(Category::Hud), 3, &methods, "rebuild/hud-1.json", "hud-1", Some("Pixel art"));
        assert!(text.contains("asset_generate_batch") && text.contains("style \"Pixel art\"") && text.contains("Never feed the original file"), "{text}");
        assert!(text.contains("never use paid services"));
        assert!(!text.contains("voice description"), "no audio rows, no audio words");
        let methods = [(Kind::Sound, LOCAL_AI), (Kind::Texture, "keep")].into_iter().collect();
        let text = brief(Some(Category::Characters), 4, &methods, "rebuild/c.json", "c", Some("Dark fantasy"));
        for words in [
            "only to understand what it is",
            "one voice description per character",
            "Never feed the original audio into a generator",
            "Keep lengths close to the originals",
            "kind voice",
            "asset_decode_sound",
        ] {
            assert!(text.contains(words), "{words}: {text}");
        }
    }

    #[test]
    fn every_plan_is_free_and_there_is_no_paid_method() {
        assert!(METHODS.iter().all(|m| m.id != "tripo"), "no paid route");
        let enemies = [item("e", Kind::Model, Category::Enemies), item("f", Kind::Model, Category::Enemies)];
        let refs: Vec<&Item> = enemies.iter().collect();
        let plan = Plan::new(&refs, None, &BTreeMap::new(), Ready::default()).unwrap();
        assert_eq!(plan.category, Some(Category::Enemies));
        assert_eq!(plan.rows[0].method, "blender");
        assert_eq!(plan.to_json()["total"], "Free");
        let mut chosen = BTreeMap::new();
        chosen.insert("model".to_string(), "tripo".to_string());
        assert!(Plan::new(&refs, None, &chosen, Ready::default()).is_err(), "Tripo isn't a method any more");
        chosen.insert("model".to_string(), "engine_ui".to_string());
        assert!(Plan::new(&refs, None, &chosen, Ready::default()).is_err(), "not offered for an enemy model");
    }
}
