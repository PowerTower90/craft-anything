//! Sorting a project's assets into categories (what each is for in the game) and planning how to
//! rebuild a category cheaply. Starting a rebuild is the AI layer's `project_inventory_rebuild`
//! (it hands the work to the project's builder). See [`crate::asset_categories`] and
//! [`crate::asset_rebuild`].

use super::{load_for_change, open};
use crate::asset_categories::{self, Category, CategoryBy, CategoryRule, glob_matches};
use crate::asset_rebuild::{self, Plan, TRIPO_CREDITS_PER_MODEL};
use crate::inbox::now_ms;
use crate::inventory::{self, Item, Source};
use crate::tool::{Ctx, Effects, Requires, ToolOutput, ToolSpec, Toolset, parse_params, schema_of};
use craft_anything_evidence::ToolError;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Most items one categorize call changes, and most terms one rules call adds.
const MAX_CHANGE: usize = 20_000;
const MAX_RULES: usize = 500;

pub fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            id: "project_inventory_categorize",
            title: "Sort assets into a category",
            summary: "Put inventory items into a category (characters, enemies, weapons, items, machines, hud, environment, effects, music, unsorted) by id or by a name glob (`*` and `?`, any case, e.g. `Glyphid*`). An agent's choice (by: agent, the default) is kept on rescans but doesn't replace the person's (by: user). For many items with a shared word, project_inventory_category_rules is usually better. Returns how many changed and up to 50 of their names.",
            beginner: "Sorts assets into a group, such as enemies or the HUD.",
            toolset: Toolset::Projects,
            effects: Effects::WRITES_FILESYSTEM,
            requires: Requires::Nothing,
            input_schema: schema_of::<CategorizeParams>,
            run: project_inventory_categorize,
        },
        ToolSpec {
            id: "project_inventory_category_rules",
            title: "Set a project's category rules",
            summary: "List, add or remove the project's own category terms: a word (`glyphid`) or words in order (`bugs small`) matched against item names, paths and origins (split on _ - spaces and camelCase; plurals and longer words that begin with the term match too), any case. They win over the built-in keywords, and the term with the most words wins (`enter drop pod` over `drop pod`). Music and fonts keep their category whatever the rules say, and items sorted by a person or an agent keep theirs. The rules are applied again at once. Returns the rules and the count of items in each category.",
            beginner: "Teaches the asset sorter this project's own names.",
            toolset: Toolset::Projects,
            effects: Effects::WRITES_FILESYSTEM,
            requires: Requires::Nothing,
            input_schema: schema_of::<RulesParams>,
            run: project_inventory_category_rules,
        },
        ToolSpec {
            id: "project_inventory_unsorted_terms",
            title: "Find words among unsorted assets",
            summary: "The most common words in the names of a project's Unsorted inventory items (each item counted once per word; numbers, hashes and file jargon left out), with how many items have each and up to 3 example names. A few project_inventory_category_rules from these sort thousands of items.",
            beginner: "Shows the words that would sort the rest of the assets.",
            toolset: Toolset::Projects,
            effects: Effects::READ_ONLY.union(Effects::CARRIES_TARGET_CONTENT),
            requires: Requires::Nothing,
            input_schema: schema_of::<TermsParams>,
            run: project_inventory_unsorted_terms,
        },
        ToolSpec {
            id: "project_inventory_rebuild_plan",
            title: "Plan an asset rebuild",
            summary: "How a category (or items by id) would be rebuilt, cheapest first: for each kind of asset among them, how many, the method (`methods` chooses per kind, e.g. {\"model\": \"blender\"}), its cost and whether it can be used yet, with every method offered. HUD → built in the engine; environment → engine materials and Blender; effects → engine particles; items, machines, weapons → Blender; characters and enemies → Blender, or Tripo with the person's web credits (an estimate from assets.tripo_credits_per_model; not set up yet); sounds → keep the original (local generation not set up yet). Nothing is written. Start it with project_inventory_rebuild.",
            beginner: "Shows how a group of assets would be remade, and what it costs.",
            toolset: Toolset::Projects,
            effects: Effects::READ_ONLY,
            requires: Requires::Nothing,
            input_schema: schema_of::<PlanParams>,
            run: project_inventory_rebuild_plan,
        },
    ]
}

/// The categorize tool's `by`.
#[derive(Debug, Clone, Copy, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum By {
    #[default]
    Agent,
    User,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CategorizeParams {
    /// The project's folder.
    pub project: PathBuf,
    /// Items by id (from project_inventory_list).
    #[serde(default)]
    pub ids: Vec<String>,
    /// Items whose name matches this glob (`*` any run, `?` one character; any case).
    #[serde(default)]
    pub name_glob: Option<String>,
    /// Their category.
    pub category: Category,
    /// Who is choosing: an agent (default) or the person.
    #[serde(default)]
    pub by: By,
}

fn visible(clean: bool) -> impl Fn(&Item) -> bool {
    move |i: &Item| !clean || i.source != Source::Original
}

fn project_inventory_categorize(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let p: CategorizeParams = parse_params(params)?;
    if p.ids.is_empty() && p.name_glob.as_deref().is_none_or(|g| g.trim().is_empty()) {
        return Err(ToolError::invalid("Say which items: ids or a name_glob.").with_remediation("project_inventory_list shows the ids and names."));
    }
    if p.ids.len() > MAX_CHANGE {
        return Err(ToolError::invalid(format!("At most {MAX_CHANGE} ids at a time.")));
    }
    let see = visible(ctx.session.clean_room());
    let ws = open(&p.project)?;
    let mut inv = load_for_change(&ws)?;
    let by = match p.by {
        By::Agent => CategoryBy::Agent,
        By::User => CategoryBy::User,
    };
    let (mut changed, mut kept_for_person, mut names) = (0usize, 0usize, Vec::new());
    for item in inv.items.iter_mut().filter(|i| see(i)) {
        let named = p.ids.contains(&item.id) || p.name_glob.as_deref().is_some_and(|g| glob_matches(g.trim(), &item.name));
        if !named {
            continue;
        }
        if by == CategoryBy::Agent && item.category_by == CategoryBy::User {
            kept_for_person += 1;
            continue;
        }
        item.category = p.category;
        item.category_by = by;
        changed += 1;
        if names.len() < 50 {
            names.push(item.name.clone());
        }
    }
    if changed > 0 {
        inventory::save(&ws.root, &inv)?;
    }
    Ok(ToolOutput::Value(json!({"changed": changed, "kept_as_the_person_chose": kept_for_person, "names": names, "category": p.category})))
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RulesParams {
    /// The project's folder.
    pub project: PathBuf,
    /// Terms to add (or to move to another category).
    #[serde(default)]
    pub add: Vec<CategoryRule>,
    /// Terms to remove (any case).
    #[serde(default)]
    pub remove: Vec<String>,
}

fn by_category(inv: &inventory::Inventory, see: &impl Fn(&Item) -> bool) -> BTreeMap<&'static str, usize> {
    let mut m = BTreeMap::new();
    for i in inv.items.iter().filter(|i| see(i)) {
        *m.entry(i.category.id()).or_insert(0) += 1;
    }
    m
}

fn project_inventory_category_rules(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let p: RulesParams = parse_params(params)?;
    let see = visible(ctx.session.clean_room());
    let ws = open(&p.project)?;
    if p.add.is_empty() && p.remove.is_empty() {
        let inv = inventory::load(&ws.root)?;
        return Ok(ToolOutput::Value(json!({"rules": inv.category_rules, "by_category": by_category(&inv, &see)})));
    }
    if p.add.iter().any(|r| asset_categories::words(&r.term).is_empty() || r.term.chars().count() > 80) {
        return Err(ToolError::invalid("Each term needs a letter or digit, and at most 80 characters."));
    }
    let mut inv = load_for_change(&ws)?;
    let same = |a: &str, b: &str| asset_categories::words(a) == asset_categories::words(b);
    inv.category_rules.retain(|r| !p.remove.iter().any(|t| same(t, &r.term)) && !p.add.iter().any(|a| same(&a.term, &r.term)));
    // Newest first, so a term just added wins over an older one it overlaps.
    let mut rules = p.add.clone();
    rules.extend(std::mem::take(&mut inv.category_rules));
    if rules.len() > MAX_RULES {
        return Err(ToolError::invalid(format!("A project keeps at most {MAX_RULES} terms.")).with_remediation("Remove some, or use broader terms."));
    }
    inv.category_rules = rules;
    asset_categories::apply(&mut inv);
    inventory::save(&ws.root, &inv)?;
    Ok(ToolOutput::Value(json!({"rules": inv.category_rules, "by_category": by_category(&inv, &see)})))
}

fn d_terms() -> usize {
    30
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TermsParams {
    /// The project's folder.
    pub project: PathBuf,
    /// How many words (1–200, default 30).
    #[serde(default = "d_terms")]
    pub limit: usize,
}

fn project_inventory_unsorted_terms(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let p: TermsParams = parse_params(params)?;
    let see = visible(ctx.session.clean_room());
    let ws = open(&p.project)?;
    let inv = inventory::load(&ws.root)?;
    let unsorted = inv.items.iter().filter(|i| see(i) && i.category == Category::Unsorted).count();
    let terms: Vec<Value> = asset_categories::unsorted_terms(&inv, p.limit.clamp(1, 200), &see)
        .into_iter()
        .map(|(term, items, examples)| json!({"term": term, "items": items, "examples": examples}))
        .collect();
    Ok(ToolOutput::Value(json!({"unsorted": unsorted, "terms": terms})))
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanParams {
    /// The project's folder.
    pub project: PathBuf,
    /// The category to rebuild.
    #[serde(default)]
    pub category: Option<Category>,
    /// Or these items (ids from project_inventory_list).
    #[serde(default)]
    pub ids: Vec<String>,
    /// A method per kind (kind id → method id, e.g. {"model": "blender"}); the rest get their default.
    #[serde(default)]
    pub methods: BTreeMap<String, String>,
}

/// The plan for these params, and the inventory it's of.
pub fn plan_for(ctx: &Ctx<'_>, p: &PlanParams) -> Result<(crate::workspace::Workspace, inventory::Inventory, Plan), ToolError> {
    let see = visible(ctx.session.clean_room());
    let ws = open(&p.project)?;
    let inv = inventory::load(&ws.root)?;
    let plan = {
        let items = asset_rebuild::select(&inv, p.category, &p.ids, &see)?;
        Plan::new(&items, p.category, &p.methods)?
    };
    Ok((ws, inv, plan))
}

/// Tripo credits one model takes, from the settings (an estimate).
pub fn credits_per_model(ctx: &Ctx<'_>) -> u32 {
    match ctx.session.settings.assets.tripo_credits_per_model {
        0 => TRIPO_CREDITS_PER_MODEL,
        n => n,
    }
}

fn project_inventory_rebuild_plan(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let p: PlanParams = parse_params(params)?;
    let (ws, _inv, plan) = plan_for(ctx, &p)?;
    let mut v = plan.to_json(credits_per_model(ctx));
    v["project"] = json!(ws.root.display().to_string());
    Ok(ToolOutput::Value(v))
}

/// Start a plan (the AI layer's project_inventory_rebuild uses this, then hands the brief over).
pub fn start_plan(ctx: &Ctx<'_>, p: &PlanParams) -> Result<(crate::workspace::Workspace, asset_rebuild::Started, Value), ToolError> {
    let (ws, _, plan) = plan_for(ctx, p)?;
    let mut inv = load_for_change(&ws)?;
    let started = asset_rebuild::start(&ws, &mut inv, &plan, now_ms())?;
    let v = json!({
        "project": ws.root.display().to_string(),
        "list": started.list.display().to_string(),
        "started": started.items,
        "skipped": started.skipped.iter().map(|(k, m, why, n)| json!({"kind": k, "method": m, "why": why, "count": n})).collect::<Vec<_>>(),
        "brief": started.brief,
        "plan": plan.to_json(credits_per_model(ctx)),
    });
    Ok((ws, started, v))
}

#[cfg(test)]
mod tests;
