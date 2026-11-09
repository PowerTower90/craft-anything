//! A project's asset inventory as tools: scan the build (and, in Deep mode, take the original's
//! assets out into reference/assets/), list what there is, and mark what to replace. The data and
//! the build scan are in [`crate::inventory`]; taking the original apart is in [`original`].
//! Ids follow docs/tool-ids.md.

pub mod original;

use crate::inbox::now_ms;
use crate::inventory::{self, Inventory, Item, Kind, Source, Status, Thumbs};
use crate::tool::{Ctx, Effects, Requires, ToolOutput, ToolSpec, Toolset, parse_params, schema_of};
use crate::workspace::Workspace;
use craft_anything_evidence::{ErrorCode, ToolError};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// The scan tool's id (Clean room runs it for the build only).
pub const SCAN_TOOL: &str = "project_inventory_scan";
/// Most items one list call returns.
const MAX_PAGE: usize = 1000;
/// Most items one mark call changes.
const MAX_MARK: usize = 5000;

pub fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            id: SCAN_TOOL,
            title: "Scan a project's assets",
            summary: "Make or refresh a project's asset inventory (inventory.json): every picture, model, sound, font, video and engine asset in its build (app/, skipping build output such as Binaries, Intermediate, Saved, node_modules), with sizes, picture sizes, sound lengths and 256-pixel thumbnails (reference/thumbs/), and the generated assets from assets.json marked `generated`. With `original` (Deep mode only; the default there) it also takes the original game's assets out into reference/assets/<kind>/: Unity textures as PNG and sounds as the game stores them, Unreal pak textures as PNG with `permission` (you made the game or its rights holder allows unpacking it), loose pictures, sounds and models as they are; meshes and other packages are listed. At most 4 GiB and 20000 items from the original; a second scan reuses what was taken out already, so it carries on. A rescan keeps every status and note the person gave. Returns counts by source, kind and status.",
            beginner: "Lists every picture, model and sound in what you're making (and in the original), so you can see what to replace.",
            toolset: Toolset::Projects,
            effects: Effects::WRITES_FILESYSTEM.union(Effects::CARRIES_TARGET_CONTENT),
            requires: Requires::Nothing,
            input_schema: schema_of::<ScanParams>,
            run: project_inventory_scan,
        },
        ToolSpec {
            id: "project_inventory_list",
            title: "List a project's assets",
            summary: "List the items of a project's asset inventory (from project_inventory_scan), filtered by source (build, original), kind (texture, sprite, model, sound, music, font, video, map, material, engine_asset, other), status (original, generated, library, placeholder, needs_replacing, replaced) and a text query (name, path, origin or note, any case), paged with offset/limit. Each item has its id, path, size, picture size, sound length, thumbnail, status and note. In Clean room the original's items are left out.",
            beginner: "Shows the assets in what you're making, with their pictures.",
            toolset: Toolset::Projects,
            effects: Effects::READ_ONLY.union(Effects::CARRIES_TARGET_CONTENT),
            requires: Requires::Nothing,
            input_schema: schema_of::<ListParams>,
            run: project_inventory_list,
        },
        ToolSpec {
            id: "project_inventory_mark",
            title: "Mark a project's assets",
            summary: "Set the status (needs_replacing, replaced, placeholder, library, generated, original) and optionally a note of inventory items by id (from project_inventory_list). Rescans keep what was set. Returns the updated items and any ids not found.",
            beginner: "Marks assets you want replaced, or have replaced.",
            toolset: Toolset::Projects,
            effects: Effects::WRITES_FILESYSTEM,
            requires: Requires::Nothing,
            input_schema: schema_of::<MarkParams>,
            run: project_inventory_mark,
        },
    ]
}

fn d_true() -> bool {
    true
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScanParams {
    /// The project's folder.
    pub project: PathBuf,
    /// Also take the original's assets out (Deep mode only). Default: on in Deep mode, off in Clean room.
    #[serde(default)]
    pub original: Option<bool>,
    /// Scan the build (app/). Default true.
    #[serde(default = "d_true")]
    pub rebuild: bool,
    /// The person made the original or its rights holder allows unpacking it: needed to take
    /// textures out of Unreal paks (as game_unreal_texture_export asks).
    #[serde(default)]
    pub permission: bool,
}

fn open(project: &Path) -> Result<Workspace, ToolError> {
    Workspace::open(project).map_err(|e| {
        ToolError::new(ErrorCode::NotFound, format!("{} isn't a project folder: {}", project.display(), e.message))
            .with_remediation("Pass a folder from project_list (it holds craft.json).")
    })
}

/// The inventory, refusing one this build mustn't change.
fn load_for_change(ws: &Workspace) -> Result<Inventory, ToolError> {
    if ws.is_read_only() {
        return Err(ToolError::unsupported(format!("{} was made by a newer Craft Anything, so this one doesn't change it.", ws.manifest.name))
            .with_remediation("Update Craft Anything. Nothing was written."));
    }
    let inv = inventory::load(&ws.root)?;
    if inv.is_newer() {
        return Err(ToolError::unsupported(format!("{} was written by a newer Craft Anything, so this one doesn't change it.", inventory::INVENTORY))
            .with_remediation("Update Craft Anything. Nothing was written."));
    }
    Ok(inv)
}

fn project_inventory_scan(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let p: ScanParams = parse_params(params)?;
    let clean = ctx.session.clean_room();
    let want_original = p.original.unwrap_or(!clean);
    if want_original && clean {
        return Err(crate::clean_room::refusal(SCAN_TOOL)
            .with_remediation("Turn on Deep mode to take the original's assets out (`--deep` on the CLI), or pass original: false to scan only the build."));
    }
    let ws = open(&p.project)?;
    let mut inv = load_for_change(&ws)?;
    let started = now_ms();
    let mut thumbs = Thumbs::new(&ws.root);
    let mut build = json!({"scanned": false});
    if p.rebuild {
        ctx.report(None, format!("Looking through {}", ws.app_place()));
        let report = |n: usize, at: &str| ctx.report(None, format!("{n} assets so far ({at})"));
        let found = inventory::scan_build(&ws, &mut thumbs, &ctx.cancel, &report)?;
        build = json!({"scanned": true, "items": found.items.len(), "notes": found.notes});
        inv.replace(Source::Build, found.items);
        inv.notes.insert(Source::Build, found.notes);
        inv.build_scanned_ms = Some(now_ms());
    }
    let mut original = json!({"scanned": false});
    if want_original {
        match original::scan(ctx, &ws, &mut thumbs, p.permission) {
            Ok(found) => {
                original = json!({
                    "scanned": true,
                    "folder": found.folder.display().to_string(),
                    "items": found.items.len(),
                    "taken_out": found.taken_out,
                    "already_there": found.already_there,
                    "bytes": found.bytes,
                    "byte_budget": original::BYTE_BUDGET,
                    "notes": found.notes,
                });
                inv.replace(Source::Original, found.items);
                inv.notes.insert(Source::Original, found.notes);
                inv.original_scanned_ms = Some(now_ms());
                inv.original_folder = Some(found.folder.display().to_string());
            }
            Err(e) if e.code == ErrorCode::NotFound => {
                // No original to read (a website, a description, a moved game): the build's
                // inventory still saves, and the original's items from before are kept.
                original = json!({"scanned": false, "notes": [e.message.clone()]});
                inv.notes.insert(Source::Original, vec![e.message]);
            }
            Err(e) => return Err(e),
        }
    }
    // A mark made while this scan ran (another window, the CLI) isn't lost.
    if let Ok(disk) = inventory::load(&ws.root) {
        inv.keep_newer_marks(&disk);
    }
    inventory::save(&ws.root, &inv)?;
    let counts = inv.counts();
    Ok(ToolOutput::Value(json!({
        "inventory": ws.root.join(inventory::INVENTORY).display().to_string(),
        "counts": counts,
        "build": build,
        "original": original,
        "thumbnails": {"made": thumbs.made, "over_budget": thumbs.over_budget},
        "took_ms": now_ms().saturating_sub(started),
    })))
}

fn d_limit() -> usize {
    100
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListParams {
    /// The project's folder.
    pub project: PathBuf,
    /// Only items of this source.
    #[serde(default)]
    pub source: Option<Source>,
    /// Only items of this kind.
    #[serde(default)]
    pub kind: Option<Kind>,
    /// Only items with this status.
    #[serde(default)]
    pub status: Option<Status>,
    /// Only items whose name, path, origin or note contains this (any case).
    #[serde(default)]
    pub query: Option<String>,
    /// First item of the page.
    #[serde(default)]
    pub offset: usize,
    /// Items per page (1–1000, default 100).
    #[serde(default = "d_limit")]
    pub limit: usize,
}

fn project_inventory_list(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let p: ListParams = parse_params(params)?;
    let clean = ctx.session.clean_room();
    let ws = open(&p.project)?;
    let inv = inventory::load(&ws.root)?;
    let needle = p.query.as_deref().map(str::to_lowercase).filter(|q| !q.trim().is_empty());
    let hidden = if clean { inv.of(Source::Original).count() } else { 0 };
    let shown: Vec<&Item> = inv.items.iter().filter(|i| !clean || i.source != Source::Original).collect();
    let matching: Vec<&Item> = shown
        .iter()
        .copied()
        .filter(|i| p.source.is_none_or(|s| i.source == s) && p.kind.is_none_or(|k| i.kind == k) && p.status.is_none_or(|s| i.status == s))
        .filter(|i| {
            needle.as_deref().is_none_or(|n| {
                [Some(i.name.as_str()), i.path.as_deref(), i.origin.as_deref(), Some(i.note.as_str())]
                    .into_iter()
                    .flatten()
                    .any(|t| t.to_lowercase().contains(n))
            })
        })
        .collect();
    let page: Vec<&Item> = matching.iter().copied().skip(p.offset).take(p.limit.clamp(1, MAX_PAGE)).collect();
    let mut counts = Inventory { items: shown.iter().map(|i| (*i).clone()).collect(), ..Inventory::default() }.counts();
    if clean {
        counts["original_hidden_in_clean_room"] = json!(hidden);
    }
    Ok(ToolOutput::Value(json!({
        "project": ws.root.display().to_string(),
        "build_scanned_ms": inv.build_scanned_ms,
        "original_scanned_ms": if clean { None } else { inv.original_scanned_ms },
        "counts": counts,
        "matching": matching.len(),
        "offset": p.offset,
        "items": page,
    })))
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MarkParams {
    /// The project's folder.
    pub project: PathBuf,
    /// The items to change (ids from project_inventory_list).
    pub ids: Vec<String>,
    /// Their new status.
    pub status: Status,
    /// A note for each of them; leave out to keep their notes, "" to clear them.
    #[serde(default)]
    pub note: Option<String>,
}

fn project_inventory_mark(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let p: MarkParams = parse_params(params)?;
    if p.ids.is_empty() || p.ids.len() > MAX_MARK {
        return Err(ToolError::invalid(format!("ids needs 1 to {MAX_MARK} item ids.")).with_remediation("project_inventory_list shows the ids."));
    }
    if p.note.as_ref().is_some_and(|n| n.chars().count() > 2000) {
        return Err(ToolError::invalid("note can be at most 2000 characters."));
    }
    let clean = ctx.session.clean_room();
    let ws = open(&p.project)?;
    let mut inv = load_for_change(&ws)?;
    let at = now_ms();
    let mut updated = Vec::new();
    let mut not_found = Vec::new();
    for id in &p.ids {
        let found = inv.items.iter_mut().find(|i| &i.id == id && (!clean || i.source != Source::Original));
        match found {
            Some(item) => {
                item.status = p.status;
                if let Some(note) = &p.note {
                    item.note = note.clone();
                }
                item.marked_ms = Some(at);
                updated.push(item.clone());
            }
            None => not_found.push(id.clone()),
        }
    }
    if !updated.is_empty() {
        inventory::save(&ws.root, &inv)?;
    }
    Ok(ToolOutput::Value(json!({"updated": updated, "not_found": not_found})))
}

#[cfg(test)]
mod tests;
