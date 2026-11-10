//! Things: what a project's asset files add up to (an enemy, a character with its outfits, a
//! voice group), listed and corrected. See [`crate::asset_things`].

use super::categories::By;
use super::{load_for_change, open};
use crate::asset_categories::{self, Category, CategoryBy};
use crate::asset_things::{self, Role, ThingRef};
use crate::inventory::{self, Item, Source};
use crate::tool::{Ctx, Effects, Requires, ToolOutput, ToolSpec, Toolset, parse_params, schema_of};
use craft_anything_evidence::ToolError;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Most files one call moves.
const MAX_CHANGE: usize = 20_000;

pub fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            id: "project_inventory_things",
            title: "List a project's things",
            summary: "What the project's asset files add up to, per category: things (an enemy, a character class with its outfits, a weapon), each with its files counted by role (model, model part, colour, normal map, mask, alpha, animation data, effect mesh, icon, sound, voice line, music…), its outfits or versions, and its preview (a model's render, else a colour texture, else an icon); then sound groups (sounds or voice lines with no model). Technical files (normal maps, masks, alpha, animation data) are never things of their own. With `key`, one thing's files grouped by role. Returns the counts per category (things and files) and the files in no thing.",
            beginner: "Shows what the game's files add up to: its enemies, characters and weapons, each with its files.",
            toolset: Toolset::Projects,
            effects: Effects::IDEMPOTENT,
            requires: Requires::Nothing,
            input_schema: schema_of::<ThingsParams>,
            run: project_inventory_things,
        },
        ToolSpec {
            id: "project_inventory_thing_set",
            title: "Group assets into a thing",
            summary: "Correct the grouping: move files (`ids`) into a thing (`thing`: its key, or a new name), with a `variant` (an outfit) or `part`; set their `role`; rename a thing (`thing` + `name`); or merge a thing into another (`thing` + `merge_into`). An agent's choice (by: agent, the default) is kept on rescans but doesn't replace the person's (by: user). Returns what changed and the things of the category.",
            beginner: "Puts files together as one thing, or renames and merges things.",
            toolset: Toolset::Projects,
            effects: Effects::WRITES_FILESYSTEM,
            requires: Requires::Nothing,
            input_schema: schema_of::<ThingSetParams>,
            run: project_inventory_thing_set,
        },
    ]
}

fn visible(clean: bool) -> impl Fn(&Item) -> bool {
    move |i: &Item| !clean || i.source != Source::Original
}

fn d_limit() -> usize {
    200
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ThingsParams {
    /// The project's folder.
    pub project: PathBuf,
    /// One category (default all).
    #[serde(default)]
    pub category: Option<Category>,
    /// One thing: its files, grouped by role.
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub offset: usize,
    /// How many things (1–1000, default 200).
    #[serde(default = "d_limit")]
    pub limit: usize,
}

/// The counts per category: things and files.
pub fn counts_json(inv: &inventory::Inventory, see: &impl Fn(&Item) -> bool) -> Value {
    let (all, _) = asset_things::things(inv.items.iter().filter(|i| see(i)), None);
    let after = asset_things::count_after(&all);
    let c: serde_json::Map<String, Value> = asset_things::counts(inv, see)
        .into_iter()
        .map(|(cat, (things, files))| (cat.id().to_string(), json!({"things": things, "after": after.get(&cat).copied().unwrap_or(0), "files": files})))
        .collect();
    Value::Object(c)
}

fn thing_json(t: &asset_things::Thing) -> Value {
    let badge: Vec<Value> = asset_things::badge(t).into_iter().map(|(what, n)| json!({"what": what, "count": n})).collect();
    json!({
        "key": t.key, "name": t.name, "category": t.category, "files": t.files, "roles": t.roles, "variants": t.variants,
        "preview": t.preview, "preview_item": t.preview_item, "sounds_only": t.sounds_only, "has_model": t.has_model, "chosen": t.chosen, "badge": badge,
        "after": t.after, "after_preview": t.after_preview, "jobs": t.jobs,
    })
}

fn project_inventory_things(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let p: ThingsParams = parse_params(params)?;
    let see = visible(ctx.session.clean_room());
    let ws = open(&p.project)?;
    let inv = inventory::load(&ws.root)?;
    if let Some(key) = p.key.as_deref() {
        let mut by_role: BTreeMap<&'static str, Vec<Value>> = BTreeMap::new();
        let mut name = None;
        for i in inv.items.iter().filter(|i| see(i) && i.thing.as_ref().is_some_and(|t| t.key == key)) {
            name = i.thing.as_ref().map(|t| t.name.clone());
            let role = i.role.unwrap_or_else(|| asset_things::role_of(i));
            let t = i.thing.as_ref();
            by_role.entry(role.id()).or_default().push(json!({
                "id": i.id, "name": i.name, "kind": i.kind, "path": i.path, "thumb": i.thumb,
                "variant": t.and_then(|t| t.variant.clone()), "part": t.and_then(|t| t.part.clone()), "status": i.status,
            }));
        }
        if by_role.is_empty() {
            return Err(ToolError::new(craft_anything_evidence::ErrorCode::NotFound, format!("There's no thing {key:?}."))
                .with_remediation("project_inventory_things lists the things and their keys."));
        }
        return Ok(ToolOutput::Value(json!({"key": key, "name": name, "files_by_role": by_role})));
    }
    let (list, loose) = asset_things::things(inv.items.iter().filter(|i| see(i)), p.category);
    let total = list.len();
    let things = asset_things::counts(&inv, &see).into_iter().filter(|(c, _)| p.category.is_none_or(|x| x == *c)).map(|(_, (t, _))| t).sum::<usize>();
    let sound_groups = list.iter().filter(|t| t.sounds_only).count();
    let page: Vec<Value> = list.iter().skip(p.offset).take(p.limit.clamp(1, 1000)).map(thing_json).collect();
    let after = asset_things::count_after(&list).into_values().sum::<usize>();
    Ok(ToolOutput::Value(json!({
        "things": things, "after": after, "sound_groups": sound_groups, "files_in_no_thing": loose,
        "total": total, "offset": p.offset, "list": page, "by_category": counts_json(&inv, &see),
    })))
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ThingSetParams {
    /// The project's folder.
    pub project: PathBuf,
    /// The thing: its key (from project_inventory_things), or a name for a new one.
    pub thing: String,
    /// Files to move into it (ids from project_inventory_list or project_inventory_things).
    #[serde(default)]
    pub ids: Vec<String>,
    /// Their outfit or version.
    #[serde(default)]
    pub variant: Option<String>,
    /// The model part they are (body, backpack…).
    #[serde(default)]
    pub part: Option<String>,
    /// Their role: model, model_part, colour, normal_map, mask, alpha, animation_data,
    /// effect_mesh, icon, sound, voice_line, music, font, video, other.
    #[serde(default)]
    pub role: Option<Role>,
    /// A new name for the thing.
    #[serde(default)]
    pub name: Option<String>,
    /// Merge the thing into this one (its key).
    #[serde(default)]
    pub merge_into: Option<String>,
    /// Who decides: agent (default) or user.
    #[serde(default)]
    pub by: By,
}

fn project_inventory_thing_set(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let p: ThingSetParams = parse_params(params)?;
    let clean = ctx.session.clean_room();
    let ws = open(&p.project)?;
    set_thing(&ws, &p, clean).map(ToolOutput::Value)
}

/// Apply one grouping change (see [`ThingSetParams`]) to the project's inventory and save it.
pub fn set_thing(ws: &crate::workspace::Workspace, p: &ThingSetParams, clean: bool) -> Result<Value, ToolError> {
    let see = visible(clean);
    let mut inv = load_for_change(ws)?;
    let by = match p.by {
        By::Agent => CategoryBy::Agent,
        By::User => CategoryBy::User,
    };
    if p.thing.trim().is_empty() || p.thing.chars().count() > 120 {
        return Err(ToolError::invalid("Say the thing: its key, or a name (up to 120 characters)."));
    }
    if p.ids.len() > MAX_CHANGE {
        return Err(ToolError::invalid(format!("At most {MAX_CHANGE} files at once.")));
    }
    // The thing as it is, or a new one in the files' category.
    let existing = inv.items.iter().filter_map(|i| i.thing.as_ref()).find(|t| t.key == p.thing).cloned();
    let mut changed = Vec::new();
    let mut kept = 0;
    if !p.ids.is_empty() {
        let category = inv.items.iter().find(|i| p.ids.contains(&i.id)).map(|i| i.category).unwrap_or(Category::Unsorted);
        let words = asset_categories::words(&p.thing);
        let (key, name) = match &existing {
            Some(t) => (t.key.clone(), t.name.clone()),
            None => (format!("{}:{}", category.id(), words.concat()), p.thing.trim().to_string()),
        };
        if existing.is_none() && words.is_empty() {
            return Err(ToolError::invalid("A thing's name needs a letter or digit."));
        }
        for i in inv.items.iter_mut().filter(|i| see(i) && p.ids.contains(&i.id)) {
            if by == CategoryBy::Agent && (i.thing_by == CategoryBy::User || i.role_by == CategoryBy::User) {
                kept += 1;
                continue;
            }
            let part = p.part.clone().or_else(|| i.thing.as_ref().and_then(|t| t.part.clone()));
            i.thing = Some(ThingRef { key: key.clone(), name: name.clone(), variant: p.variant.clone(), part, sounds_only: false });
            i.thing_by = by;
            if let Some(r) = p.role {
                i.role = Some(r);
                i.role_by = by;
            }
            changed.push(i.name.clone());
        }
    }
    if let Some(n) = p.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        let key = existing.as_ref().map(|t| t.key.clone()).unwrap_or_else(|| p.thing.clone());
        inv.thing_names.insert(key, n.chars().take(120).collect());
    }
    if let Some(into) = p.merge_into.as_deref() {
        let from = existing.as_ref().map(|t| t.key.clone()).ok_or_else(|| ToolError::invalid(format!("There's no thing {:?} to merge.", p.thing)))?;
        if !inv.items.iter().any(|i| i.thing.as_ref().is_some_and(|t| t.key == into)) {
            return Err(ToolError::invalid(format!("There's no thing {into:?} to merge into.")).with_remediation("project_inventory_things lists the keys."));
        }
        if from != into {
            inv.thing_merges.insert(from, into.to_string());
        }
    }
    asset_categories::apply(&mut inv);
    inventory::save(&ws.root, &inv)?;
    Ok(json!({
        "changed": changed.len(), "kept_as_the_person_chose": kept, "names": changed.iter().take(50).collect::<Vec<_>>(),
        "by_category": counts_json(&inv, &see),
    }))
}

#[cfg(test)]
mod tests {
    use crate::inventory::{INVENTORY_VERSION, Inventory, Kind, Status};
    use crate::workspace::{TargetRef, Workspace};
    use crate::{CallOptions, Caller, Engine};
    use serde_json::{Value, json};

    fn call(e: &Engine, tool: &str, args: Value) -> Value {
        e.call(tool, args, CallOptions::new(Caller::Mcp)).unwrap().to_json()["result"].clone()
    }

    #[test]
    fn things_are_listed_moved_renamed_and_merged_and_a_person_wins() {
        let d = tempfile::tempdir().unwrap();
        let ws = Workspace::create(d.path(), "Game", "A game", TargetRef::None).unwrap();
        let items = ["Warden_Model", "Warden_Mat_Base", "Warden_Mat_Normal", "MiniBrood_Model", "MiniBrood_MAT_Base", "Odd_Thing_Normal"]
            .iter()
            .map(|n| {
                let k = if n.ends_with("Model") { Kind::Model } else { Kind::Texture };
                let mut i = crate::inventory::Item::new(crate::inventory::Source::Build, n, k, *n, Status::Placeholder);
                i.id = n.to_ascii_lowercase();
                i.category = crate::asset_categories::Category::Enemies;
                i.category_by = crate::asset_categories::CategoryBy::User;
                i
            })
            .collect();
        let mut inv = Inventory { version: INVENTORY_VERSION, items, ..Default::default() };
        crate::asset_categories::apply(&mut inv);
        crate::inventory::save(&ws.root, &inv).unwrap();
        let e = Engine::new(crate::testing::session());
        let r = call(&e, "project_inventory_things", json!({"project": ws.root, "category": "enemies"}));
        assert_eq!((r["things"].as_u64(), r["files_in_no_thing"].as_u64()), (Some(2), Some(1)), "{r}");
        assert_eq!(r["by_category"]["enemies"], json!({"things": 2, "after": 0, "files": 6}));
        let one = call(&e, "project_inventory_things", json!({"project": ws.root, "key": "enemies:warden"}));
        assert_eq!(one["files_by_role"]["normal_map"][0]["name"], "Warden_Mat_Normal");
        // The person moves the odd map to the warden; an agent can't move it back.
        let r = call(&e, "project_inventory_thing_set", json!({"project": ws.root, "thing": "enemies:warden", "ids": ["odd_thing_normal"], "by": "user"}));
        assert_eq!(r["changed"], 1);
        let r = call(&e, "project_inventory_thing_set", json!({"project": ws.root, "thing": "enemies:minibrood", "ids": ["odd_thing_normal"]}));
        assert_eq!((r["changed"].as_u64(), r["kept_as_the_person_chose"].as_u64()), (Some(0), Some(1)));
        // Rename, then merge.
        call(&e, "project_inventory_thing_set", json!({"project": ws.root, "thing": "enemies:warden", "name": "The Warden"}));
        call(&e, "project_inventory_thing_set", json!({"project": ws.root, "thing": "enemies:minibrood", "merge_into": "enemies:warden"}));
        let r = call(&e, "project_inventory_things", json!({"project": ws.root, "category": "enemies"}));
        assert_eq!(r["things"], 1);
        assert_eq!(r["list"][0]["name"], "The Warden");
        assert_eq!(r["list"][0]["files"], 6);
        let err = e
            .call("project_inventory_thing_set", json!({"project": ws.root, "thing": "enemies:warden", "merge_into": "nowhere"}), CallOptions::new(Caller::Mcp))
            .unwrap_err();
        assert_eq!(err.code, craft_anything_evidence::ErrorCode::BadInput);
    }
}
