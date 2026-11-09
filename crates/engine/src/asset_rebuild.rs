//! Rebuilding a group of assets as cheaply as possible: for each kind of asset in the group, the
//! ways it can be remade ([`METHODS`]), which one each category and kind starts with
//! ([`options`]), what that costs, and starting it: the list goes to
//! `<project>/rebuild/<category>-<time>.json`, the items are marked, and the builder gets a plain
//! brief ([`brief`]).
//!
//! Free methods come first everywhere. Nothing here ever spends money: a method that would use
//! someone's paid credits is offered only once it's set up, and shows what it would use.

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
/// Tripo credits one 3D model takes, when the setting doesn't say (an estimate).
pub const TRIPO_CREDITS_PER_MODEL: u32 = 40;

/// What a method costs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cost {
    Free,
    /// The person's Tripo web credits, per model.
    TripoCredits,
}

/// One way to remake an asset.
#[derive(Debug, Clone, Copy)]
pub struct Method {
    pub id: &'static str,
    pub title: &'static str,
    pub detail: &'static str,
    pub cost: Cost,
    /// Why it can't be used yet; None when it can.
    pub not_ready: Option<&'static str>,
}

/// Every method, free ones first.
pub const METHODS: &[Method] = &[
    Method {
        id: "engine_ui",
        title: "Built in the engine",
        detail: "Widgets and vector shapes drawn by the game's own code, with an open-licence font.",
        cost: Cost::Free,
        not_ready: None,
    },
    Method {
        id: "engine_materials",
        title: "Engine materials",
        detail: "Engine materials and procedural textures, with Blender where a shape is needed.",
        cost: Cost::Free,
        not_ready: None,
    },
    Method { id: "engine_particles", title: "Engine particles", detail: "The engine's own particle systems.", cost: Cost::Free, not_ready: None },
    Method { id: "blender", title: "Blender", detail: "Modelled in Blender from simple shapes.", cost: Cost::Free, not_ready: None },
    Method {
        id: "tripo",
        title: "Tripo (your web credits)",
        detail: "Generated on Tripo with the credits on your account (an estimate per model).",
        cost: Cost::TripoCredits,
        not_ready: Some("Not set up yet — next step"),
    },
    Method {
        id: "local_audio",
        title: "Local generation (free)",
        detail: "Sounds generated on this computer.",
        cost: Cost::Free,
        not_ready: Some("Not set up yet"),
    },
    Method { id: "keep", title: "Keep the original for now", detail: "Not rebuilt this time.", cost: Cost::Free, not_ready: None },
];

pub fn method(id: &str) -> Option<&'static Method> {
    METHODS.iter().find(|m| m.id == id)
}

/// The methods offered for one kind of asset in one category, the first being the default.
pub fn options(category: Category, kind: Kind) -> &'static [&'static str] {
    use Category as C;
    use Kind as K;
    match (category, kind) {
        (_, K::Sound | K::Music) => &["keep", "local_audio"],
        (_, K::Font) => &["engine_ui", "keep"],
        (C::Hud, K::Model) => &["blender", "engine_ui", "keep"],
        (C::Hud, _) => &["engine_ui", "keep"],
        (C::Environment, K::Model) => &["blender", "engine_materials", "keep"],
        (C::Environment, _) => &["engine_materials", "blender", "keep"],
        (C::Effects, _) => &["engine_particles", "keep"],
        (C::Characters | C::Enemies, K::Model) => &["blender", "tripo", "keep"],
        (C::Characters | C::Enemies, _) => &["blender", "keep"],
        (C::Items | C::Machines | C::Weapons, K::Texture | K::Sprite | K::Material) => &["engine_materials", "blender", "keep"],
        (C::Items | C::Machines | C::Weapons, _) => &["blender", "keep"],
        (C::Music, _) => &["keep", "local_audio"],
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
    pub options: &'static [&'static str],
}

impl Row {
    /// Tripo credits this row would use (0 for free methods).
    pub fn credits(&self, per_model: u32) -> u64 {
        match method(self.method).map(|m| m.cost) {
            Some(Cost::TripoCredits) => self.count as u64 * u64::from(per_model),
            _ => 0,
        }
    }

    pub fn ready(&self) -> bool {
        method(self.method).is_some_and(|m| m.not_ready.is_none())
    }
}

/// A plan: the items, and for each kind among them the method chosen.
#[derive(Debug, Clone)]
pub struct Plan {
    /// The category the items are in, when they're all in one.
    pub category: Option<Category>,
    pub ids: Vec<String>,
    pub rows: Vec<Row>,
}

impl Plan {
    /// The plan for `items` (all in `category`, if given), each kind with `chosen` methods
    /// (kind id → method id) or its default. A method the kind doesn't offer is refused.
    pub fn new(items: &[&Item], category: Option<Category>, chosen: &BTreeMap<String, String>) -> Result<Plan, ToolError> {
        let mut by_kind: BTreeMap<Kind, usize> = BTreeMap::new();
        for i in items {
            *by_kind.entry(i.kind).or_default() += 1;
        }
        let common = category.or_else(|| items.first().map(|i| i.category).filter(|c| items.iter().all(|i| i.category == *c)));
        let group = common.unwrap_or(Category::Unsorted);
        let mut rows = Vec::new();
        for (kind, count) in by_kind {
            let offered = options(group, kind);
            let method = match chosen.get(kind.id()) {
                Some(m) => offered.iter().copied().find(|o| o == m).ok_or_else(|| {
                    ToolError::invalid(format!("{m} isn't a way to rebuild {} here.", kind.id()))
                        .with_remediation(format!("Use one of: {}.", offered.join(", ")))
                })?,
                None => offered.first().copied().unwrap_or("keep"),
            };
            rows.push(Row { kind, count, method, options: offered });
        }
        Ok(Plan { category: common, ids: items.iter().map(|i| i.id.clone()).collect(), rows })
    }

    pub fn credits(&self, per_model: u32) -> u64 {
        self.rows.iter().map(|r| r.credits(per_model)).sum()
    }

    /// "Free", or "about N Tripo credits" (an estimate).
    pub fn total_words(&self, per_model: u32) -> String {
        match self.credits(per_model) {
            0 => "Free".to_string(),
            n => format!("about {n} Tripo credits"),
        }
    }

    /// The plan as the tools return it.
    pub fn to_json(&self, per_model: u32) -> Value {
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
                            "free": m.cost == Cost::Free,
                            "credits": if m.cost == Cost::TripoCredits { r.count as u64 * u64::from(per_model) } else { 0 },
                            "available": m.not_ready.is_none(), "why": m.not_ready,
                        })
                    })
                    .collect();
                json!({"kind": r.kind, "count": r.count, "method": r.method, "available": r.ready(), "credits": r.credits(per_model), "options": options})
            })
            .collect();
        json!({
            "category": self.category,
            "items": self.ids.len(),
            "rows": rows,
            "total_credits": self.credits(per_model),
            "total": self.total_words(per_model),
            "credits_are_estimates": true,
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
            Some(m) if m.not_ready.is_some() => skipped.push((r.kind, m.id, m.not_ready.unwrap_or_default().to_string(), r.count)),
            Some(m) => {
                going.insert(r.kind, m.id);
            }
            None => {}
        }
    }
    let mut listed = Vec::new();
    for item in inv.items.iter_mut().filter(|i| plan.ids.contains(&i.id)) {
        let Some(m) = going.get(&item.kind) else { continue };
        item.status = Status::NeedsReplacing;
        item.marked_ms = Some(now_ms);
        item.rebuild = Some(RebuildMark { method: m.to_string(), requested_ms: now_ms, job: job.clone() });
        listed.push(json!({
            "id": item.id, "name": item.name, "kind": item.kind, "category": item.category,
            "path": item.path, "origin": item.origin, "method": m,
        }));
    }
    if listed.is_empty() {
        return Ok(Started { list: path, items: 0, skipped, brief: String::new() });
    }
    let doc = json!({
        "version": 1,
        "category": plan.category,
        "created_ms": now_ms,
        "project": ws.root.display().to_string(),
        "methods": going.iter().map(|(k, m)| (k.id().to_string(), json!(m))).collect::<serde_json::Map<String, Value>>(),
        "items": listed,
        "skipped": skipped.iter().map(|(k, m, why, n)| json!({"kind": k, "method": m, "why": why, "count": n})).collect::<Vec<_>>(),
    });
    std::fs::create_dir_all(ws.root.join(REBUILD_DIR)).map_err(|e| ToolError::io(ws.root.join(REBUILD_DIR).display(), e))?;
    let bytes = serde_json::to_vec_pretty(&doc).map_err(|e| ToolError::internal(e.to_string()))?;
    write_atomic(&path, &bytes)?;
    inventory::save(&ws.root, inv)?;
    let count = listed.len();
    Ok(Started { brief: brief(plan.category, count, &going, &rel), list: path, items: count, skipped })
}

/// The builder's brief for a started rebuild. Fixed English: it's for the agent.
pub fn brief(category: Option<Category>, count: usize, methods: &BTreeMap<Kind, &'static str>, list: &str) -> String {
    let what = category.map_or_else(|| "the selected".to_string(), |c| format!("the {}", c.title()));
    let how: Vec<String> = methods.iter().map(|(k, m)| format!("{} → {}", k.id(), method(m).map_or(*m, |x| x.title))).collect();
    format!(
        "Rebuild {what} assets: {count} items. Methods: {}. The full list, with each item's method, is in {list} (in the project folder). \
         Use free methods only: never use paid services or spend money. Keep the game working while you replace them. \
         Mark each item Replaced with project_inventory_mark (status replaced, its id) when its replacement is in the build.",
        how.join("; ")
    )
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
    fn the_hud_is_free_and_tripo_is_offered_but_not_ready() {
        let hud = [item("a", Kind::Texture, Category::Hud), item("b", Kind::Font, Category::Hud), item("c", Kind::Sound, Category::Hud)];
        let refs: Vec<&Item> = hud.iter().collect();
        let plan = Plan::new(&refs, Some(Category::Hud), &BTreeMap::new()).unwrap();
        assert_eq!(
            plan.rows.iter().map(|r| (r.kind, r.method)).collect::<Vec<_>>(),
            [(Kind::Texture, "engine_ui"), (Kind::Sound, "keep"), (Kind::Font, "engine_ui")]
        );
        assert_eq!(plan.total_words(40), "Free");
        let enemies = [item("e", Kind::Model, Category::Enemies), item("f", Kind::Model, Category::Enemies)];
        let refs: Vec<&Item> = enemies.iter().collect();
        let mut chosen = BTreeMap::new();
        chosen.insert("model".to_string(), "tripo".to_string());
        let plan = Plan::new(&refs, None, &chosen).unwrap();
        assert_eq!(plan.category, Some(Category::Enemies));
        assert_eq!(plan.total_words(40), "about 80 Tripo credits");
        assert!(!plan.rows[0].ready(), "Tripo isn't set up");
        chosen.insert("model".to_string(), "engine_ui".to_string());
        assert!(Plan::new(&refs, None, &chosen).is_err(), "not offered for an enemy model");
    }
}
