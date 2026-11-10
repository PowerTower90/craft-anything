//! Asset categories in the app: the category list (the gallery's main filter), a category per
//! item, and rebuilding a category (or a selection) the cheapest way, through the project's own
//! builder. The sorting rules, the methods and their costs are the engine's
//! ([`craft_anything_engine::asset_categories`], [`craft_anything_engine::asset_rebuild`]); this
//! draws them and calls their tools on workers.

use crate::assets_ui::{Summary, count, kind_words};
use crate::i18n::{tr, trf};
use crate::state::Screen;
use crate::theme::{self, Tokens};
use crate::{CraftApp, Done, icons, widgets};
use craft_anything_engine::evidence::ToolError;
use craft_anything_engine::inventory::{Category, CategoryBy, Item, Kind};
use craft_anything_engine::{CallOptions, Caller, Consent, Engine};
use egui::{Margin, RichText, Sense, Ui, Vec2};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// What the builder is asked when the person wants the Unsorted assets sorted. Fixed words (not
/// translated): they're for the agent.
pub const SORT_BRIEF: &str = "Sort this project's Unsorted assets into categories (characters, enemies, weapons, items, machines, hud, environment, effects, music). \
Call project_inventory_unsorted_terms to see the most common words among them, then add project terms with project_inventory_category_rules \
(a few terms usually sort thousands of items; they win over the built-in keywords), and use project_inventory_categorize for the odd ones. \
Repeat until few are left. Don't change items a person sorted. This costs no money: don't use paid services. Say what you sorted when you're done.";

pub fn category_icon(c: Category) -> &'static str {
    match c {
        Category::Characters => "user",
        Category::Enemies => "bug",
        Category::Weapons => "wrench",
        Category::Items => "package",
        Category::Machines => "box",
        Category::Hud => "app-window",
        Category::Environment => "globe",
        Category::Effects => "sparkles",
        Category::Music => "music",
        Category::Unsorted => "circle-help",
    }
}

/// One way to remake a kind, as project_inventory_rebuild_plan gives it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PlanOption {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub detail: String,
    pub available: bool,
    #[serde(default)]
    pub why: Option<String>,
}

/// One kind of asset in a plan.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PlanRow {
    pub kind: Kind,
    pub count: usize,
    pub method: String,
    pub available: bool,
    pub options: Vec<PlanOption>,
}

/// A plan, as project_inventory_rebuild_plan gives it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PlanView {
    #[serde(default)]
    pub category: Option<Category>,
    pub items: usize,
    pub rows: Vec<PlanRow>,
    /// Some kind is made with local AI (then the style is asked for).
    #[serde(default)]
    pub uses_local_ai: bool,
    /// The style the plan has (the project's last one at first).
    #[serde(default)]
    pub style: Option<String>,
    /// What the items add up to.
    #[serde(default)]
    pub things: PlanThings,
}

/// A plan's things, as project_inventory_rebuild_plan gives them.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct PlanThings {
    pub things: usize,
    pub with_model: usize,
    pub sounds: usize,
    pub made_with_model: usize,
    pub other_versions: usize,
    pub noun: String,
}

/// The plan dialog: what it's for, the methods chosen, and the plan once read.
#[derive(Debug, Clone, Default)]
pub struct PlanState {
    pub root: PathBuf,
    pub category: Option<Category>,
    pub ids: Vec<String>,
    /// Kind id → method id the person chose.
    pub methods: BTreeMap<String, String>,
    pub view: Option<Result<PlanView, String>>,
    pub loading: bool,
    pub starting: bool,
    /// The style for local AI: a preset or the person's own words.
    pub style: String,
    /// Describe the items with the Assets AI before the rebuild starts (asset_describe), so the
    /// builder uses those descriptions.
    pub describe_first: bool,
    /// Every outfit or version of each thing, not only its main one.
    pub outfits: bool,
}

fn call(engine: &Engine, tool: &str, params: Value, job: &crate::jobs::JobCtx) -> Result<Value, ToolError> {
    let opts = CallOptions::new(Caller::Ui).with_consent(Consent::Granted).with_cancel(job.cancel.clone());
    engine.call(tool, params, opts).map(|o| o.to_json().get("result").cloned().unwrap_or(Value::Null))
}

impl CraftApp {
    /// Show the plan for rebuilding a category of the open project's assets, or items by id.
    pub fn open_rebuild_plan(&mut self, category: Option<Category>, ids: Vec<String>) {
        let Some(root) = self.workspace.as_ref().map(|w| w.root.clone()) else { return };
        let describe_first = self.local_assets_ready();
        self.assets.plan = Some(PlanState { root, category, ids, describe_first, ..Default::default() });
        self.load_rebuild_plan();
    }

    /// Change one kind's method in the open plan (it's read again).
    pub fn choose_rebuild_method(&mut self, kind: Kind, method: &str) {
        if let Some(p) = self.assets.plan.as_mut() {
            p.methods.insert(kind.id().to_string(), method.to_string());
        }
        self.load_rebuild_plan();
    }

    /// Read the open plan (project_inventory_rebuild_plan) on a worker.
    fn load_rebuild_plan(&mut self) {
        let Some(p) = self.assets.plan.as_mut() else { return };
        p.loading = true;
        let style = Some(p.style.trim()).filter(|s| !s.is_empty());
        let params = json!({"project": p.root, "category": p.category, "ids": p.ids, "methods": p.methods, "style": style, "outfits": p.outfits});
        let (engine, root) = (self.engine.clone(), p.root.clone());
        self.jobs.spawn("Plan a rebuild", false, move |j| {
            let result = call(&engine, "project_inventory_rebuild_plan", params, j)
                .and_then(|v| serde_json::from_value::<PlanView>(v).map_err(|e| ToolError::internal(format!("The plan couldn't be read: {e}"))));
            Ok(Done::AssetPlan { root, result })
        });
    }

    /// Start the open plan: project_inventory_rebuild writes the list and marks the items, then
    /// the brief goes to the project's builder the way a typed change does.
    pub fn start_asset_rebuild(&mut self) {
        let Some(p) = self.assets.plan.as_mut().filter(|p| !p.starting) else { return };
        p.starting = true;
        let style = Some(p.style.trim()).filter(|s| !s.is_empty());
        let params =
            json!({"project": p.root, "category": p.category, "ids": p.ids, "methods": p.methods, "style": style, "outfits": p.outfits, "send": false});
        let describe = p.describe_first.then(|| json!({"project": p.root, "category": p.category, "ids": p.ids, "limit": 5000}));
        let (engine, root) = (self.engine.clone(), p.root.clone());
        self.jobs.spawn(tr("Start a rebuild"), true, move |j| {
            // Described first, so the list the builder gets has the descriptions; a description
            // that fails leaves that item to the builder.
            if let Some(d) = describe {
                let progress = j.reporter();
                let opts = CallOptions::new(Caller::Ui).with_consent(Consent::Granted).with_cancel(j.cancel.clone()).with_progress(&progress);
                if let Err(e) = engine.call("asset_describe", d, opts) {
                    log::warn!("describing before the rebuild failed: {}", e.message);
                }
            }
            let result = call(&engine, "project_inventory_rebuild", params, j);
            Ok(Done::AssetRebuildStarted { root, result })
        });
    }

    /// Put items in a category, as the person's choice (project_inventory_categorize, by: user).
    pub fn categorize_assets(&mut self, ids: Vec<String>, category: Category) {
        let Some(root) = self.workspace.as_ref().map(|w| w.root.clone()) else { return };
        if ids.is_empty() {
            return;
        }
        let engine = self.engine.clone();
        self.jobs.spawn("Sort assets", false, move |j| {
            let result = call(&engine, "project_inventory_categorize", json!({"project": root, "ids": ids, "category": category, "by": "user"}), j);
            Ok(Done::AssetsCategorized { root, ids, category, result })
        });
    }

    /// Sort the Unsorted assets with the Assets AI: an API backend (a local model) proposes terms
    /// itself (project_inventory_sort_with_agent); the builder gets the sorting brief ([`SORT_BRIEF`]).
    pub fn sort_unsorted_with_agent(&mut self) {
        let Some(root) = self.workspace.as_ref().map(|w| w.root.clone()) else { return };
        let api =
            self.assets_backend.as_ref().is_some_and(|n| self.backends.iter().any(|b| &b.name == n && b.kind != craft_anything_ai::BackendKind::AgentCli));
        if api {
            self.act("project_inventory_sort_with_agent", json!({"project": root}), tr("Sort the unsorted with the AI"));
            return self.toast(tr("The AI is sorting them; the counts change when it's done."), false);
        }
        self.deliver(root, SORT_BRIEF.to_string(), Vec::new(), Ok(Vec::new()));
        self.toast(tr("Sent to the builder. Its sorting shows here as it goes."), false);
    }

    /// A plan, rebuild or sorting worker finished.
    pub(crate) fn asset_plan_done(&mut self, done: Done) {
        match done {
            Done::AssetPlan { root, result } => {
                if let Some(p) = self.assets.plan.as_mut().filter(|p| p.root == root) {
                    p.loading = false;
                    if let (true, Ok(v)) = (p.style.trim().is_empty(), &result) {
                        p.style = v.style.clone().unwrap_or_default();
                    }
                    p.view = Some(result.map_err(|e| e.for_people()));
                }
            }
            Done::AssetRebuildStarted { root, result } => {
                if let Some(p) = self.assets.plan.as_mut() {
                    p.starting = false;
                }
                match result {
                    Ok(v) => {
                        let n = v["started"].as_u64().unwrap_or(0);
                        let brief = v["brief"].as_str().unwrap_or_default().to_string();
                        self.assets.plan = None;
                        if n == 0 || brief.is_empty() {
                            return self.toast(tr("Nothing to rebuild: every kind is kept or not set up yet."), true);
                        }
                        self.toast(trf("{n} assets sent to the builder.", &[("n", &count(n))]), false);
                        // The marks changed: read the list and the counts again.
                        self.assets.summary = None;
                        if self.assets.root.as_ref() == Some(&root) {
                            self.load_assets();
                        }
                        self.count_assets(&root);
                        if self.workspace.as_ref().is_some_and(|w| w.root == root) {
                            self.deliver(root, brief, Vec::new(), Ok(Vec::new()));
                        }
                    }
                    Err(e) => self.toast(e.for_people(), true),
                }
            }
            Done::AssetsCategorized { root, ids, category, result } => match result {
                Ok(_) => {
                    if self.assets.root.as_ref() == Some(&root) {
                        self.assets.set_category(&ids, category, CategoryBy::User);
                    }
                    if self.assets.summary.as_ref().is_some_and(|s| s.root == root) {
                        self.assets.summary = None;
                        self.count_assets(&root);
                    }
                }
                Err(e) => self.toast(e.for_people(), true),
            },
            _ => {}
        }
    }
}

/// The gallery's category list: icon, name, count and Rebuild… for each category with items;
/// a click shows only that category (again: all of them).
pub fn category_list(app: &mut CraftApp, ui: &mut Ui) {
    let t = Tokens::get(ui.ctx());
    widgets::caption(ui, "Category");
    let mut pick = None;
    let mut rebuild = None;
    for c in Category::ALL {
        let n = app.assets.category_count(c);
        let on = app.assets.category == Some(c);
        if n == 0 && !on {
            continue;
        }
        // One row exactly as wide as the rail: the name and count, then Rebuild… at the end. (A
        // row wider than the rail would widen it, every frame, moving the grid under the pointer.)
        let (full, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 26.0), Sense::hover());
        let rebuild_words = tr("Rebuild…");
        let rg = ui.painter().layout_no_wrap(rebuild_words, egui::FontId::proportional(theme::CAPTION), t.accent_ink);
        let rebuild_w = if n > 0 { rg.size().x + 14.0 } else { 0.0 };
        let rect = egui::Rect::from_min_max(full.min, egui::pos2((full.right() - rebuild_w - 4.0).max(full.left() + 40.0), full.bottom()));
        let resp = ui.interact(rect, egui::Id::new(("asset-category", c.id())), Sense::click());
        // Things when the cards are shown ("Enemies 19"), files when every file is.
        let things = !app.assets.show_files;
        let shown_n = if things { app.assets.category_things(c) } else { n };
        // One string for both what's painted and what's announced, so they can't disagree (the
        // painted number once stayed the file count while the label said the things).
        let shown = count(shown_n as u64);
        let label = format!("{} {}", tr(c.title()), shown);
        resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, on, &label));
        let fill = if on { t.accent_soft } else { theme::mix(egui::Color32::TRANSPARENT, t.hover, widgets::hover_t(ui, &resp)) };
        ui.painter().rect_filled(rect, t.r_sm(), fill);
        icons::paint(
            ui,
            egui::Rect::from_min_size(rect.min + Vec2::new(6.0, 5.0), Vec2::splat(16.0)),
            category_icon(c),
            14.0,
            if on { t.accent_ink } else { t.icon },
        );
        let num = ui.painter().layout_no_wrap(shown, theme::mono(theme::CAPTION), t.text_faint);
        let name_clip = egui::Rect::from_min_max(rect.min, egui::pos2(rect.right() - num.size().x - 10.0, rect.bottom()));
        let name = ui.painter().layout_no_wrap(tr(c.title()), egui::FontId::proportional(theme::CAPTION), t.text);
        ui.painter().with_clip_rect(name_clip).galley(egui::pos2(rect.left() + 28.0, rect.center().y - name.size().y / 2.0), name, t.text);
        ui.painter().galley(egui::pos2(rect.right() - num.size().x - 6.0, rect.center().y - num.size().y / 2.0), num, t.text_faint);
        let resp = if things { resp.on_hover_text(trf("{n} files", &[("n", &count(n as u64))])) } else { resp };
        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            pick = Some(c);
        }
        if n > 0 {
            let rr = egui::Rect::from_min_max(egui::pos2(full.right() - rebuild_w, full.top()), full.max);
            let r = ui.interact(rr, egui::Id::new(("asset-category-rebuild", c.id())), Sense::click());
            let name = trf("Rebuild {category}…", &[("category", &tr(c.title()))]);
            r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &name));
            let h = widgets::hover_t(ui, &r);
            ui.painter().rect_filled(rr, t.r_sm(), theme::mix(egui::Color32::TRANSPARENT, t.hover, h));
            ui.painter().galley(rr.center() - rg.size() / 2.0, rg, t.accent_ink);
            widgets::focus_ring(ui, &r, rr);
            if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                rebuild = Some(c);
            }
        }
    }
    if app.assets.category_count(Category::Unsorted) > 0
        && widgets::secondary(ui, Some("sparkles"), "Sort the unsorted with the agent")
            .on_hover_text(tr("The AI that does the asset work sorts them (a model on this computer when there is one, else the builder). It costs no money."))
            .clicked()
    {
        app.sort_unsorted_with_agent();
    }
    // Models with a file and no picture yet: Blender renders them, so the gallery shows them.
    let unrendered = app.assets.items.iter().filter(|i| i.kind == Kind::Model && i.path.is_some() && i.thumb.is_none()).count();
    if unrendered > 0 {
        if app.assets.rendering {
            ui.add(widgets::Spinner::new().size(14.0).color(t.accent));
        } else if widgets::secondary(ui, Some("box"), "Render model previews")
            .on_hover_text(trf("Blender makes a picture of {n} models, so they show here.", &[("n", &count(unrendered as u64))]))
            .clicked()
        {
            app.render_model_previews();
        }
    }
    if let Some(c) = pick {
        app.assets.category = if app.assets.category == Some(c) { None } else { Some(c) };
        app.assets.kind = None;
    }
    if let Some(c) = rebuild {
        app.open_rebuild_plan(Some(c), Vec::new());
    }
}

/// Above the grid while a category is shown: "HUD & menus · 412 items · Rebuild…".
pub fn category_header(app: &mut CraftApp, ui: &mut Ui, category: Category, shown: usize) {
    let t = Tokens::get(ui.ctx());
    let things = app.assets.category_things(category);
    let files = app.assets.category_count(category);
    ui.horizontal(|ui| {
        icons::show(ui, category_icon(category), 16.0, t.accent);
        let words = if app.assets.show_files {
            trf("{category} · {n} items", &[("category", &tr(category.title())), ("n", &count(shown as u64))])
        } else {
            let noun = tr(craft_anything_engine::asset_things::noun(category));
            trf(
                "{category} · {n} {things} · {files} files",
                &[("category", &tr(category.title())), ("n", &count(things as u64)), ("things", &noun), ("files", &count(files as u64))],
            )
        };
        ui.label(RichText::new(words).font(theme::semibold(theme::BODY)));
        if widgets::link(ui, "Rebuild…").clicked() {
            app.open_rebuild_plan(Some(category), Vec::new());
        }
    });
    ui.add_space(4.0);
}

/// A category picker; returns the category picked this frame.
fn category_combo(ui: &mut Ui, salt: &str, current: Option<Category>, name: &str) -> Option<Category> {
    let mut pick = None;
    let shown = current.map_or_else(|| tr("Category…"), |c| tr(c.title()));
    let r = egui::ComboBox::from_id_salt(("asset-category-combo", salt)).icon(widgets::combo_icon).selected_text(shown).width(170.0).height(420.0).show_ui(
        ui,
        |ui| {
            for c in Category::ALL {
                if ui.selectable_label(current == Some(c), tr(c.title())).clicked() {
                    pick = Some(c);
                }
            }
        },
    );
    widgets::named(r.response, name);
    pick
}

/// The focused item's category, to change (the person's choice wins over the rules and agents).
pub fn item_category(app: &mut CraftApp, ui: &mut Ui, item: &Item) {
    let t = Tokens::get(ui.ctx());
    widgets::caption(ui, "Category");
    ui.horizontal(|ui| {
        if let Some(c) = category_combo(ui, "item", Some(item.category), "Category of this asset") {
            app.categorize_assets(vec![item.id.clone()], c);
        }
        let by = match item.category_by {
            CategoryBy::Rule => "sorted by the rules",
            CategoryBy::Agent => "sorted by the agent",
            CategoryBy::User => "sorted by you",
        };
        ui.label(RichText::new(tr(by)).color(t.text_faint).size(theme::CAPTION));
    });
}

/// For several selected items: a category for all of them, and Rebuild selected…
pub fn selection_controls(app: &mut CraftApp, ui: &mut Ui, salt: &str) {
    let ids: Vec<String> = app.assets.selected.iter().cloned().collect();
    ui.horizontal_wrapped(|ui| {
        if let Some(c) = category_combo(ui, salt, None, "Category for the selected") {
            app.categorize_assets(ids.clone(), c);
        }
        if widgets::secondary(ui, Some("hammer"), "Rebuild selected…").clicked() {
            app.open_rebuild_plan(None, ids.clone());
        }
    });
}

/// The project page's Assets card: each category, its count and Rebuild.
pub fn card_categories(app: &mut CraftApp, ui: &mut Ui, s: &Summary) {
    let t = Tokens::get(ui.ctx());
    let mut rebuild = None;
    ui.add_space(6.0);
    for c in Category::ALL {
        let n = s.by_category.get(&c).copied().unwrap_or(0);
        if n == 0 {
            continue;
        }
        // The things ("Enemies 19") over the files ("827 files").
        let things = s.things_by_category.get(&c).copied().unwrap_or(0);
        // Right to left: the button, then the icon with the name over its count, so a narrow card
        // keeps both readable (a name too long for the card is shortened, never the count).
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let r = widgets::secondary(ui, None, "Rebuild");
                if widgets::named(r, &trf("Rebuild {category}", &[("category", &tr(c.title()))])).clicked() {
                    rebuild = Some(c);
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    icons::show(ui, category_icon(c), 14.0, t.icon);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        let name = if things > 0 { format!("{} {}", tr(c.title()), count(things)) } else { tr(c.title()) };
                        ui.add(egui::Label::new(RichText::new(name).size(theme::SMALL).color(t.text)).truncate());
                        ui.label(RichText::new(trf("{n} files", &[("n", &count(n))])).font(theme::mono(theme::CAPTION)).color(t.text_faint));
                    });
                });
            });
        });
    }
    if let Some(c) = rebuild {
        app.open_rebuild_plan(Some(c), Vec::new());
    }
}

/// The style for local AI: a preset, or the person's own words.
fn style_picker(ui: &mut Ui, style: &mut String) {
    let t = Tokens::get(ui.ctx());
    ui.add_space(10.0);
    widgets::caption(ui, "Style");
    let presets = craft_anything_engine::asset_rebuild::STYLES;
    let shown = if style.trim().is_empty() {
        tr("Choose a style…")
    } else if presets.contains(&style.trim()) {
        tr(style.trim())
    } else {
        style.clone()
    };
    let mut pick = None;
    let r = egui::ComboBox::from_id_salt("asset-plan-style").icon(widgets::combo_icon).selected_text(shown).width(260.0).height(360.0).show_ui(ui, |ui| {
        for p in presets {
            if ui.selectable_label(style.trim() == *p, tr(p)).clicked() {
                pick = Some(*p);
            }
        }
    });
    widgets::named(r.response, "Style preset");
    if let Some(p) = pick {
        *style = p.to_string();
    }
    let r = ui.add(
        egui::TextEdit::singleline(style)
            .hint_text(tr("Or describe your own style…"))
            .desired_width(f32::INFINITY)
            .frame(widgets::text_field_frame_compact(&t)),
    );
    widgets::named(r, "Your own style");
    ui.label(RichText::new(tr("Every picture and model in this rebuild is made in this style.")).size(theme::CAPTION).color(t.text_faint));
}

/// The plan dialog (on every screen): per kind, how many, the method (only free ones are ready),
/// what it costs, then the total and Start.
pub fn plan_modal(app: &mut CraftApp, ctx: &egui::Context) {
    let Some(plan) = app.assets.plan.clone() else { return };
    let mut style = plan.style.clone();
    let mut describe_first = plan.describe_first;
    let mut outfits = plan.outfits;
    let assets_ai = app.assets_backend.clone();
    let t = Tokens::get(ctx);
    let mut close = false;
    let mut choose: Option<(Kind, String)> = None;
    let mut start = false;
    let resp = egui::Modal::new(egui::Id::new("asset-rebuild-plan"))
        .frame(egui::Frame::new().fill(t.card).corner_radius(t.r_lg()).inner_margin(Margin::same(24)).stroke(egui::Stroke::new(1.0, t.card_border)))
        .show(ctx, |ui| {
            ui.set_width(560.0);
            let what = match plan.category {
                Some(c) => tr(c.title()),
                None => trf("{n} selected", &[("n", &count(plan.ids.len() as u64))]),
            };
            ui.label(theme::heading(trf("Rebuild: {what}", &[("what", &what)]), theme::SECTION));
            ui.add_space(4.0);
            ui.label(RichText::new(tr("The builder remakes them with free tools. It never spends money.")).color(t.text_dim));
            ui.add_space(12.0);
            match &plan.view {
                None => {
                    ui.add(widgets::Spinner::new().color(t.accent));
                }
                Some(Err(e)) => {
                    ui.colored_label(t.danger, e);
                }
                Some(Ok(view)) => {
                    egui::Grid::new("asset-plan-rows").num_columns(4).spacing([14.0, 8.0]).show(ui, |ui| {
                        for row in &view.rows {
                            ui.label(RichText::new(tr(kind_words(row.kind))).font(theme::semibold(theme::SMALL)));
                            ui.label(RichText::new(count(row.count as u64)).font(theme::mono(theme::CAPTION)).color(t.text_dim));
                            let current = row.options.iter().find(|o| o.id == row.method);
                            let shown = current.map_or_else(|| row.method.clone(), |o| tr(&o.title));
                            let r = egui::ComboBox::from_id_salt(("asset-plan-method", row.kind.id()))
                                .icon(widgets::combo_icon)
                                .selected_text(shown)
                                .width(220.0)
                                .show_ui(ui, |ui| {
                                    for o in &row.options {
                                        let label = if o.available {
                                            tr(&o.title)
                                        } else {
                                            format!("{} · {}", tr(&o.title), tr(o.why.as_deref().unwrap_or("Not set up yet")))
                                        };
                                        let item =
                                            ui.add_enabled(o.available, egui::Button::selectable(o.id == row.method, label)).on_hover_text(tr(&o.detail));
                                        if item.clicked() {
                                            choose = Some((row.kind, o.id.clone()));
                                        }
                                    }
                                });
                            widgets::named(r.response, &trf("Method for {kind}", &[("kind", &tr(kind_words(row.kind)))]));
                            // Every method is free.
                            let cost = if row.available { tr("Free") } else { tr(current.and_then(|o| o.why.as_deref()).unwrap_or("Not set up yet")) };
                            ui.label(RichText::new(cost).color(if row.available { t.success } else { t.warning }).size(theme::CAPTION));
                            ui.end_row();
                        }
                    });
                    // Per thing: one new model each; their maps come with it.
                    let th = &view.things;
                    if th.with_model > 0 {
                        ui.add_space(8.0);
                        let noun = tr(&th.noun);
                        ui.label(
                            RichText::new(trf(
                                "{n} {things}: one new model each; their textures and maps come from the new model.",
                                &[("n", &count(th.with_model as u64)), ("things", &noun)],
                            ))
                            .size(theme::SMALL),
                        );
                    }
                    if th.made_with_model > 0 {
                        ui.label(
                            RichText::new(trf(
                                "{n} normal maps, masks and animation files are made with the models, not on their own.",
                                &[("n", &count(th.made_with_model as u64))],
                            ))
                            .size(theme::CAPTION)
                            .color(t.text_faint),
                        );
                    }
                    if th.other_versions > 0 || outfits {
                        let r = ui.checkbox(&mut outfits, tr("Every outfit, not only each one's main look"));
                        if th.other_versions > 0 {
                            r.on_hover_text(trf("{n} files of other outfits are left as they are.", &[("n", &count(th.other_versions as u64))]));
                        }
                    }
                    ui.add_space(10.0);
                    ui.label(RichText::new(trf("Total: {total}", &[("total", &tr("Free"))])).font(theme::semibold(theme::BODY)));
                    if view.uses_local_ai {
                        style_picker(ui, &mut style);
                    }
                    if let Some(name) = assets_ai.as_deref().filter(|_| view.rows.iter().any(|r| r.method != "keep")) {
                        ui.add_space(6.0);
                        ui.checkbox(&mut describe_first, tr("Describe them first with the local model")).on_hover_text(trf(
                            "{name} looks at each one and writes what it is; the builder uses those words instead of writing its own.",
                            &[("name", name)],
                        ));
                    }
                    let kept: usize = view.rows.iter().filter(|r| r.method == "keep" || !r.available).map(|r| r.count).sum();
                    if kept > 0 {
                        ui.label(
                            RichText::new(trf("{n} kept as they are this time (kept, or their method isn't set up yet).", &[("n", &count(kept as u64))]))
                                .size(theme::CAPTION)
                                .color(t.text_faint),
                        );
                    }
                }
            }
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                let ready = matches!(&plan.view, Some(Ok(v)) if v.rows.iter().any(|r| r.available && r.method != "keep"));
                if widgets::primary_icon(ui, Some("hammer"), "Start", ready && !plan.starting && !plan.loading).clicked() {
                    start = true;
                }
                if widgets::secondary(ui, None, "Cancel").clicked() {
                    close = true;
                }
                if plan.loading || plan.starting {
                    ui.add(widgets::Spinner::new().size(14.0).color(t.accent));
                }
            });
        });
    if resp.should_close() {
        close = true;
    }
    if let Some(p) = app.assets.plan.as_mut().filter(|p| p.style != style) {
        p.style = style;
    }
    if let Some(p) = app.assets.plan.as_mut() {
        p.describe_first = describe_first;
    }
    if app.assets.plan.as_ref().is_some_and(|p| p.outfits != outfits) {
        if let Some(p) = app.assets.plan.as_mut() {
            p.outfits = outfits;
        }
        app.load_rebuild_plan();
    }
    if let Some((kind, method)) = choose {
        app.choose_rebuild_method(kind, &method);
    }
    if start {
        app.start_asset_rebuild();
        // The person follows the builder on the project page.
        if app.state.screen != Screen::Workbench {
            app.state.screen = Screen::Project;
        }
    } else if close {
        app.assets.plan = None;
    }
}
