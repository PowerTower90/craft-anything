//! The asset gallery: every picture, model, sound, font and video of the open project (its
//! inventory, from `project_inventory_scan`), so the person sees what there is and marks what to
//! replace. It lives in the Workbench's Assets rail, with a small card on the project page.
//!
//! Everything slow happens on workers: the list is read through `project_inventory_list` (paged),
//! marks go through `project_inventory_mark`, a scan through `project_inventory_scan`, and the
//! thumbnails are decoded a few at a time for the tiles in view only, then kept (the least recently
//! drawn go first). Filtering ~10 000 items happens once per change of filter, never per frame.

use crate::i18n::{tr, trf};
use crate::project::Thumb;
use crate::state::{Modal, RailKind, RightTab, Screen};
use crate::theme::{self, Tokens};
use crate::{CraftApp, Done, icons, widgets};
use craft_anything_engine::evidence::ToolError;
use craft_anything_engine::inventory::{Category, Item, Kind, Source, Status};
use craft_anything_engine::tools::inventory_tools::SCAN_TOOL;
use craft_anything_engine::{CallOptions, Caller, Consent, Engine};
use egui::{Color32, Margin, RichText, Sense, Ui, Vec2};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

/// Items one list call asks for (the tool's most).
const PAGE: usize = 1000;
/// The most items the gallery reads.
const MOST: usize = 60_000;
/// Thumbnails one worker decodes, and how many workers decode at once.
const THUMB_BATCH: usize = 24;
const THUMB_WORKERS: usize = 2;
/// Thumbnail textures kept; past this, the least recently drawn are dropped.
const THUMB_CACHE: usize = 900;
/// The largest thumbnail file read.
const THUMB_FILE: u64 = 8 << 20;
/// A large preview's longest side in pixels, and the largest picture file read for it.
const PREVIEW_SIDE: u32 = 1024;
const PREVIEW_FILE: u64 = 48 << 20;
/// A tile, and the gap between tiles.
const TILE_W: f32 = 118.0;
const TILE_H: f32 = 146.0;
const GAP: f32 = 8.0;

/// The words for each kind (plural, on chips and the project card).
pub fn kind_words(k: Kind) -> &'static str {
    match k {
        Kind::Texture => "Textures",
        Kind::Sprite => "Sprites",
        Kind::Model => "Models",
        Kind::Sound => "Sounds",
        Kind::Music => "Music",
        Kind::Font => "Fonts",
        Kind::Video => "Videos",
        Kind::Map => "Maps",
        Kind::Material => "Materials",
        Kind::EngineAsset => "Engine assets",
        Kind::Other => "Other files",
    }
}

fn kind_icon(k: Kind) -> &'static str {
    match k {
        Kind::Texture | Kind::Sprite => "image",
        Kind::Model => "box",
        Kind::Sound => "volume-2",
        Kind::Music => "music",
        Kind::Font => "type",
        Kind::Video => "film",
        Kind::Map => "globe",
        Kind::Material => "layers",
        Kind::EngineAsset => "package",
        Kind::Other => "file",
    }
}

pub fn status_words(s: Status) -> &'static str {
    match s {
        Status::Original => "Original",
        Status::Generated => "Generated",
        Status::Library => "Library",
        Status::Placeholder => "Placeholder",
        Status::NeedsReplacing => "Needs replacing",
        Status::Replaced => "Replaced",
    }
}

const STATUSES: [Status; 6] = [Status::NeedsReplacing, Status::Placeholder, Status::Replaced, Status::Original, Status::Generated, Status::Library];

/// The status buttons: what each sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkAs {
    NeedsReplacing,
    Placeholder,
    /// Nothing to replace: the original's own asset stays `original`, a build's becomes `library`
    /// (a generated one stays `generated`).
    Keep,
    Replaced,
}

impl MarkAs {
    pub const ALL: [MarkAs; 4] = [MarkAs::NeedsReplacing, MarkAs::Placeholder, MarkAs::Keep, MarkAs::Replaced];

    pub fn words(self) -> &'static str {
        match self {
            MarkAs::NeedsReplacing => "Needs replacing",
            MarkAs::Placeholder => "Placeholder",
            MarkAs::Keep => "Keep",
            MarkAs::Replaced => "Replaced",
        }
    }

    /// The status this sets on `item`.
    pub fn status_for(self, item: &Item) -> Status {
        match self {
            MarkAs::NeedsReplacing => Status::NeedsReplacing,
            MarkAs::Placeholder => Status::Placeholder,
            MarkAs::Replaced => Status::Replaced,
            MarkAs::Keep => match (item.source, item.status) {
                (Source::Original, _) => Status::Original,
                (Source::Build, Status::Generated) => Status::Generated,
                (Source::Build, _) => Status::Library,
            },
        }
    }

    /// The button shows as chosen for an item with this status.
    fn is(self, s: Status) -> bool {
        match self {
            MarkAs::NeedsReplacing => s == Status::NeedsReplacing,
            MarkAs::Placeholder => s == Status::Placeholder,
            MarkAs::Replaced => s == Status::Replaced,
            MarkAs::Keep => matches!(s, Status::Original | Status::Generated | Status::Library),
        }
    }
}

/// What the project card says: how many of each kind, how many are marked to replace.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Summary {
    pub root: PathBuf,
    pub items: u64,
    pub by_kind: BTreeMap<Kind, u64>,
    /// Items in each category.
    pub by_category: BTreeMap<Category, u64>,
    /// Items marked "needs replacing".
    pub marked: u64,
    /// The build or the original was scanned (an inventory exists).
    pub scanned: bool,
    /// The original's items Clean room leaves out.
    pub hidden: u64,
}

impl Summary {
    /// From a `project_inventory_list` result.
    pub fn from_list(root: &Path, v: &Value) -> Summary {
        let counts = &v["counts"];
        let mut by_kind = BTreeMap::new();
        if let Some(sources) = counts["by_kind"].as_object() {
            for kinds in sources.values().filter_map(Value::as_object) {
                for (k, n) in kinds {
                    if let Ok(kind) = serde_json::from_value::<Kind>(json!(k)) {
                        *by_kind.entry(kind).or_insert(0u64) += n.as_u64().unwrap_or(0);
                    }
                }
            }
        }
        let by_category = counts["by_category"]
            .as_object()
            .map(|m| m.iter().filter_map(|(k, n)| Some((Category::from_id(k)?, n.as_u64().unwrap_or(0)))).collect())
            .unwrap_or_default();
        Summary {
            root: root.to_path_buf(),
            items: counts["items"].as_u64().unwrap_or(0),
            by_kind,
            by_category,
            marked: counts["by_status"][Status::NeedsReplacing.id()].as_u64().unwrap_or(0),
            scanned: !v["build_scanned_ms"].is_null() || !v["original_scanned_ms"].is_null() || counts["items"].as_u64().unwrap_or(0) > 0,
            hidden: counts["original_hidden_in_clean_room"].as_u64().unwrap_or(0),
        }
    }

    /// "Textures 1,932 · Sounds 7,320 · …", every kind there is.
    pub fn line(&self) -> String {
        Kind::ALL
            .iter()
            .filter_map(|k| self.by_kind.get(k).filter(|n| **n > 0).map(|n| format!("{} {}", tr(kind_words(*k)), count(*n))))
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

/// A number with thousands separators.
pub fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The whole list, read on a worker.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub summary: Summary,
    pub items: Vec<Item>,
}

/// Where the list stands.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum ListState {
    #[default]
    Idle,
    Loading,
    Ready,
    Failed(String),
}

/// What filters the grid; recomputed only when it changes.
#[derive(Debug, Clone, PartialEq)]
struct ViewKey {
    category: Option<Category>,
    source: Option<Source>,
    kind: Option<Kind>,
    status: Option<Status>,
    query: String,
    show_unreadable: bool,
    generation: u64,
}

/// An item the scan couldn't take out of the original: there's no file and no picture of it,
/// only why (a texture the game fills while it runs, say).
pub fn unreadable(item: &Item) -> bool {
    item.problem.is_some() && item.path.is_none()
}

/// The order the grid shows items in: those with a picture first (textures and sprites with a
/// thumbnail, by name), then the other kinds, and the ones with a problem last.
fn order_key(item: &Item) -> (bool, bool, Kind, String) {
    let preview = item.thumb.is_some() && matches!(item.kind, Kind::Texture | Kind::Sprite | Kind::Model);
    (item.problem.is_some(), !preview, item.kind, item.name.to_lowercase())
}

/// The gallery's state: the project's items, the filters, the selection and the thumbnails.
#[derive(Default)]
pub struct Gallery {
    /// The project the list and the summary are of.
    pub root: Option<PathBuf>,
    pub summary: Option<Summary>,
    counting: Option<PathBuf>,
    /// Counting failed for this project (not tried again until a scan or a mark).
    count_error: Option<(PathBuf, String)>,
    pub items: Vec<Item>,
    pub list: ListState,
    /// The category shown (the main filter).
    pub category: Option<Category>,
    pub source: Option<Source>,
    pub kind: Option<Kind>,
    pub status: Option<Status>,
    pub query: String,
    /// Show the items the scan couldn't read (off at first: they're a wall of empty tiles).
    pub show_unreadable: bool,
    key: Option<ViewKey>,
    /// Every item's index in the grid's order, and the list version it was sorted for.
    order: Vec<usize>,
    order_of: Option<u64>,
    /// Unreadable items the other filters let through (the toggle's count).
    unreadable: usize,
    /// Indexes into `items` that pass the filters, in order.
    shown: Vec<usize>,
    /// Items of each kind passing the other filters (the chips' counts).
    kinds: BTreeMap<Kind, usize>,
    /// Items in each category passing the filters other than category and kind.
    cats: BTreeMap<Category, usize>,
    generation: u64,
    pub selected: BTreeSet<String>,
    anchor: Option<String>,
    /// The tile a press selected, so the click that ends the same press doesn't select it again.
    pressed: Option<String>,
    /// The item shown in the detail panel.
    pub focus: Option<String>,
    /// The note being typed for the focused item.
    pub note: String,
    note_for: Option<String>,
    thumbs: HashMap<String, (Thumb, u64)>,
    thumb_workers: usize,
    frame: u64,
    preview: Option<(String, Thumb)>,
    /// The scan under way (its job).
    pub scanning: Option<u64>,
    /// Marks sent and not back yet.
    pub marking: usize,
    /// Model previews being rendered.
    pub rendering: bool,
    /// The rebuild plan dialog, while it's open.
    pub plan: Option<crate::asset_plan::PlanState>,
    /// The filters the grid was last scrolled for: when they change, it starts at the top.
    scrolled_for: Option<GridFilters>,
}

/// What the grid shows (category, source, kind, status, search, unreadable shown).
type GridFilters = (Option<Category>, Option<Source>, Option<Kind>, Option<Status>, String, bool);

impl Gallery {
    /// Start over for another project (or none).
    fn reset_for(&mut self, root: Option<&Path>) {
        if self.root.as_deref() != root {
            *self = Gallery { root: root.map(Path::to_path_buf), ..Default::default() };
        }
    }

    pub fn item(&self, id: &str) -> Option<&Item> {
        self.items.iter().find(|i| i.id == id)
    }

    /// The items the filters let through, in order.
    pub fn shown(&mut self) -> &[usize] {
        self.refilter();
        &self.shown
    }

    /// How many items of `category` pass the filters other than category and kind.
    pub fn category_count(&mut self, category: Category) -> usize {
        self.refilter();
        self.cats.get(&category).copied().unwrap_or(0)
    }

    /// Put items in a category here too, once the tool has saved it.
    pub(crate) fn set_category(&mut self, ids: &[String], category: Category, by: craft_anything_engine::inventory::CategoryBy) {
        for item in self.items.iter_mut().filter(|i| ids.contains(&i.id)) {
            if by == craft_anything_engine::inventory::CategoryBy::Agent && item.category_by == craft_anything_engine::inventory::CategoryBy::User {
                continue;
            }
            item.category = category;
            item.category_by = by;
        }
        self.generation += 1;
    }

    /// How many unreadable items the other filters let through.
    pub fn unreadable_count(&mut self) -> usize {
        self.refilter();
        self.unreadable
    }

    /// How many items of `kind` pass the other filters.
    pub fn kind_count(&mut self, kind: Kind) -> usize {
        self.refilter();
        self.kinds.get(&kind).copied().unwrap_or(0)
    }

    fn refilter(&mut self) {
        let key = ViewKey {
            category: self.category,
            source: self.source,
            kind: self.kind,
            status: self.status,
            query: self.query.trim().to_lowercase(),
            show_unreadable: self.show_unreadable,
            generation: self.generation,
        };
        if self.key.as_ref() == Some(&key) {
            return;
        }
        if self.order_of != Some(self.generation) || self.order.len() != self.items.len() {
            let mut order: Vec<usize> = (0..self.items.len()).collect();
            order.sort_by_cached_key(|i| self.items.get(*i).map(order_key));
            self.order = order;
            self.order_of = Some(self.generation);
        }
        self.shown.clear();
        self.kinds.clear();
        self.cats.clear();
        self.unreadable = 0;
        for &i in &self.order {
            let Some(item) = self.items.get(i) else { continue };
            let pass = key.source.is_none_or(|s| item.source == s)
                && key.status.is_none_or(|s| item.status == s)
                && (key.query.is_empty()
                    || [Some(item.name.as_str()), item.path.as_deref(), item.origin.as_deref(), Some(item.note.as_str())]
                        .into_iter()
                        .flatten()
                        .any(|t| t.to_lowercase().contains(&key.query)));
            if !pass {
                continue;
            }
            let in_category = key.category.is_none_or(|c| item.category == c);
            if unreadable(item) {
                self.unreadable += usize::from(in_category && key.kind.is_none_or(|k| item.kind == k));
                if !key.show_unreadable {
                    continue;
                }
            }
            *self.cats.entry(item.category).or_insert(0) += 1;
            if !in_category {
                continue;
            }
            *self.kinds.entry(item.kind).or_insert(0) += 1;
            if key.kind.is_none_or(|k| item.kind == k) {
                self.shown.push(i);
            }
        }
        self.key = Some(key);
    }

    /// Put marked items' new status and note in place.
    fn apply(&mut self, updated: Vec<Item>) {
        let mut by_id: HashMap<String, Item> = updated.into_iter().map(|i| (i.id.clone(), i)).collect();
        for item in &mut self.items {
            if let Some(new) = by_id.remove(&item.id) {
                *item = new;
            }
        }
        self.generation += 1;
    }

    /// Drop the least recently drawn thumbnails past the cache's size.
    fn evict(&mut self) {
        if self.thumbs.len() <= THUMB_CACHE {
            return;
        }
        let mut old: Vec<(u64, String)> =
            self.thumbs.iter().filter(|(_, (t, _))| !matches!(t, Thumb::Loading)).map(|(id, (_, at))| (*at, id.clone())).collect();
        old.sort();
        let excess = self.thumbs.len().saturating_sub(THUMB_CACHE * 3 / 4);
        for (_, id) in old.into_iter().take(excess) {
            self.thumbs.remove(&id);
        }
    }
}

/// A relative path inside the project, refused if it climbs out of it.
fn inside(root: &Path, rel: &str) -> Option<PathBuf> {
    let p = Path::new(rel);
    let ok = !rel.is_empty() && p.components().all(|c| matches!(c, std::path::Component::Normal(_) | std::path::Component::CurDir));
    ok.then(|| root.join(p))
}

fn is_picture_file(rel: &str) -> bool {
    let lower = rel.to_ascii_lowercase();
    [".png", ".jpg", ".jpeg", ".webp", ".gif", ".bmp", ".tga"].iter().any(|e| lower.ends_with(e))
}

/// A picture file as a texture at most `side` pixels on its longer side.
fn load_picture(ctx: &egui::Context, path: &Path, name: String, side: u32, most_bytes: u64) -> Option<egui::TextureHandle> {
    use craft_anything_engine::tools::web_common::{decode_picture, resize};
    let small = std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() <= most_bytes);
    let img = small.then(|| std::fs::read(path).ok()).flatten().and_then(|b| decode_picture(&b).ok()).filter(|i| i.width > 0 && i.height > 0)?;
    let longest = img.width.max(img.height);
    let img = if longest > side {
        let scaled = |s: u32| u32::try_from(u64::from(s) * u64::from(side) / u64::from(longest)).unwrap_or(side).max(1);
        resize(&img, scaled(img.width), scaled(img.height))
    } else {
        img
    };
    let ci = egui::ColorImage::from_rgba_unmultiplied([img.width as usize, img.height as usize], &img.rgba);
    Some(ctx.load_texture(name, ci, egui::TextureOptions::LINEAR))
}

/// Call a tool from a worker, as the person.
fn call(engine: &Engine, tool: &str, params: Value, job: &crate::jobs::JobCtx) -> Result<Value, ToolError> {
    let progress = job.reporter();
    let opts = CallOptions::new(Caller::Ui).with_consent(Consent::Granted).with_cancel(job.cancel.clone()).with_progress(&progress);
    engine.call(tool, params, opts).map(|o| o.to_json().get("result").cloned().unwrap_or(Value::Null))
}

/// Every item of the project, page by page.
fn read_all(engine: &Engine, root: &Path, job: &crate::jobs::JobCtx) -> Result<Loaded, ToolError> {
    let mut items: Vec<Item> = Vec::new();
    let mut summary = None;
    loop {
        let v = call(engine, "project_inventory_list", json!({"project": root, "offset": items.len(), "limit": PAGE}), job)?;
        let matching = v["matching"].as_u64().unwrap_or(0) as usize;
        let page: Vec<Item> = serde_json::from_value(v["items"].clone()).map_err(|e| ToolError::internal(format!("The asset list couldn't be read: {e}")))?;
        let got = page.len();
        items.extend(page);
        summary.get_or_insert_with(|| Summary::from_list(root, &v));
        job.report(craft_anything_engine::Progress {
            fraction: (matching > 0).then(|| items.len() as f32 / matching as f32),
            message: format!("{} / {}", items.len(), matching),
        });
        if got == 0 || items.len() >= matching || items.len() >= MOST || job.cancel.is_cancelled() {
            break;
        }
    }
    Ok(Loaded { summary: summary.unwrap_or_else(|| Summary { root: root.to_path_buf(), ..Default::default() }), items })
}

impl CraftApp {
    /// Read the open project's asset list (through project_inventory_list) for the gallery.
    pub fn load_assets(&mut self) {
        let Some(root) = self.workspace.as_ref().map(|w| w.root.clone()) else { return };
        self.assets.reset_for(Some(&root));
        self.assets.list = ListState::Loading;
        let engine = self.engine.clone();
        self.jobs.spawn(tr("Read the asset list"), true, move |j| {
            let result = read_all(&engine, &root, j);
            Ok(Done::AssetsListed { root, result })
        });
    }

    /// Count the project's assets for its card (one small project_inventory_list call), once per
    /// project unless asked again.
    pub fn count_assets(&mut self, root: &Path) {
        if self.assets.counting.as_deref() == Some(root)
            || self.assets.summary.as_ref().is_some_and(|s| s.root == root)
            || self.assets.count_error.as_ref().is_some_and(|(r, _)| r == root)
        {
            return;
        }
        self.assets.counting = Some(root.to_path_buf());
        let (engine, root) = (self.engine.clone(), root.to_path_buf());
        self.jobs.spawn("Count the assets", false, move |j| {
            let result = call(&engine, "project_inventory_list", json!({"project": root, "limit": 1}), j).map(|v| Summary::from_list(&root, &v));
            Ok(Done::AssetsCounted { root, result })
        });
    }

    /// Scan the open project's assets (project_inventory_scan): its build, and in Deep mode the
    /// original's too. Shown in Jobs with its progress; the list is read again when it finishes.
    pub fn scan_assets(&mut self) {
        let Some(root) = self.workspace.as_ref().map(|w| w.root.clone()) else { return };
        if self.assets.scanning.is_some() {
            return;
        }
        let engine = self.engine.clone();
        let params = json!({"project": root});
        self.assets.scanning = Some(self.jobs.spawn(tr("Scan the assets"), true, move |j| {
            let result = call(&engine, SCAN_TOOL, params, j);
            Ok(Done::Acted { tool: SCAN_TOOL, result })
        }));
    }

    /// Set the status (and optionally the note) of items (project_inventory_mark). `MarkAs::Keep`
    /// can mean a different status per item, so one call goes per status.
    pub fn mark_assets(&mut self, ids: &[String], mark: MarkAs, note: Option<String>) {
        let Some(root) = self.workspace.as_ref().map(|w| w.root.clone()) else { return };
        let mut groups: BTreeMap<Status, Vec<String>> = BTreeMap::new();
        for id in ids {
            if let Some(item) = self.assets.item(id) {
                groups.entry(mark.status_for(item)).or_default().push(id.clone());
            }
        }
        for (status, ids) in groups {
            self.assets.marking += 1;
            let (engine, root, note) = (self.engine.clone(), root.clone(), note.clone());
            self.jobs.spawn("Mark assets", false, move |j| {
                let mut params = json!({"project": root, "ids": ids, "status": status});
                if let Some(n) = note {
                    params["note"] = json!(n);
                }
                let result = call(&engine, "project_inventory_mark", params, j);
                Ok(Done::AssetsMarked { root, result })
            });
        }
    }

    /// The note typed for the focused item, saved with its status as it is.
    fn save_asset_note(&mut self) {
        let Some(item) = self.assets.focus.as_deref().and_then(|id| self.assets.item(id)).cloned() else { return };
        let Some(root) = self.workspace.as_ref().map(|w| w.root.clone()) else { return };
        let note = self.assets.note.trim().to_string();
        self.assets.marking += 1;
        let engine = self.engine.clone();
        self.jobs.spawn("Mark assets", false, move |j| {
            let params = json!({"project": root, "ids": [item.id], "status": item.status, "note": note});
            let result = call(&engine, "project_inventory_mark", params, j);
            Ok(Done::AssetsMarked { root, result })
        });
    }

    /// Show the open project's assets: the Workbench's Assets rail.
    pub fn open_asset_gallery(&mut self) {
        self.state.screen = Screen::Workbench;
        self.state.rail = RailKind::Assets;
        self.follow.off = None;
        // Read again: a build or a scan elsewhere may have changed the inventory.
        if self.assets.list != ListState::Loading {
            self.assets.list = ListState::Idle;
        }
    }

    /// Show a file in the system's file manager (its folder), on a worker.
    fn reveal_asset(&mut self, path: PathBuf) {
        self.jobs.spawn("Open file location", false, move |_| {
            let folder = path.parent().map(Path::to_path_buf).unwrap_or(path);
            craft_anything_platform::open_path(&folder).map_err(|e| e.message)?;
            Ok(Done::Nothing)
        });
    }

    /// Decode thumbnails for these tiles (id, thumbnail path) on workers, a batch at a time.
    fn want_asset_thumbs(&mut self, ctx: &egui::Context, want: Vec<(String, String)>) {
        let Some(root) = self.assets.root.clone() else { return };
        let mut want: Vec<(String, String)> = want.into_iter().filter(|(id, _)| !self.assets.thumbs.contains_key(id)).collect();
        while self.assets.thumb_workers < THUMB_WORKERS && !want.is_empty() {
            let batch: Vec<(String, String)> = want.drain(..want.len().min(THUMB_BATCH)).collect();
            let frame = self.assets.frame;
            for (id, _) in &batch {
                self.assets.thumbs.insert(id.clone(), (Thumb::Loading, frame));
            }
            self.assets.thumb_workers += 1;
            let (ctx, root) = (ctx.clone(), root.clone());
            self.jobs.spawn("Load thumbnails", false, move |_| {
                let thumbs = batch
                    .into_iter()
                    .map(|(id, rel)| {
                        let image = inside(&root, &rel)
                            .and_then(|p| load_picture(&ctx, &p, format!("asset:{id}"), craft_anything_engine::inventory::THUMB_SIDE, THUMB_FILE));
                        (id, image)
                    })
                    .collect();
                Ok(Done::AssetThumbs { root, thumbs })
            });
        }
    }

    /// A large picture of the focused item: the picture itself when it's one, else its thumbnail.
    fn want_asset_preview(&mut self, ctx: &egui::Context, item: &Item) {
        if self.assets.preview.as_ref().is_some_and(|(id, _)| *id == item.id) {
            return;
        }
        let Some(root) = self.assets.root.clone() else { return };
        let rel = item.path.clone().filter(|p| is_picture_file(p)).or_else(|| item.thumb.clone());
        let Some(path) = rel.and_then(|r| inside(&root, &r)) else {
            self.assets.preview = Some((item.id.clone(), Thumb::Missing));
            return;
        };
        self.assets.preview = Some((item.id.clone(), Thumb::Loading));
        let (ctx, id) = (ctx.clone(), item.id.clone());
        self.jobs.spawn("Load a picture", false, move |_| {
            let image = load_picture(&ctx, &path, format!("asset-preview:{id}"), PREVIEW_SIDE, PREVIEW_FILE);
            Ok(Done::AssetPreview { root, id, image })
        });
    }

    /// A gallery worker finished.
    pub(crate) fn assets_done(&mut self, done: Done) {
        match done {
            Done::AssetsListed { root, result } => {
                if self.assets.root.as_deref() != Some(&root) {
                    return;
                }
                match result {
                    Ok(loaded) => {
                        self.assets.summary = Some(loaded.summary);
                        self.assets.items = loaded.items;
                        self.assets.list = ListState::Ready;
                        self.assets.generation += 1;
                        let ids: BTreeSet<&str> = self.assets.items.iter().map(|i| i.id.as_str()).collect();
                        self.assets.selected.retain(|id| ids.contains(id.as_str()));
                    }
                    Err(e) => self.assets.list = ListState::Failed(e.for_people()),
                }
            }
            Done::AssetsCounted { root, result } => {
                if self.assets.counting.as_deref() == Some(&root) {
                    self.assets.counting = None;
                }
                match result {
                    Ok(s) if self.workspace.as_ref().is_some_and(|w| w.root == root) => self.assets.summary = Some(s),
                    Ok(_) => {}
                    Err(e) => {
                        self.log(true, format!("Couldn't count the assets: {}", e.message));
                        self.assets.count_error = Some((root, e.for_people()));
                    }
                }
            }
            Done::AssetsMarked { root, result } => {
                self.assets.marking = self.assets.marking.saturating_sub(1);
                match result {
                    Ok(v) => {
                        let updated: Vec<Item> = serde_json::from_value(v["updated"].clone()).unwrap_or_default();
                        if self.assets.root.as_deref() == Some(&root) {
                            if let Some(id) = &self.assets.note_for
                                && let Some(item) = updated.iter().find(|i| &i.id == id)
                            {
                                self.assets.note = item.note.clone();
                            }
                            self.assets.apply(updated);
                        }
                        // The card's "marked to replace" is counted again.
                        if self.assets.summary.as_ref().is_some_and(|s| s.root == root) {
                            self.assets.summary = None;
                        }
                        self.assets.count_error = None;
                        if self.assets.marking == 0 {
                            self.count_assets(&root);
                        }
                    }
                    Err(e) => self.toast(e.for_people(), true),
                }
            }
            Done::AssetThumbs { root, thumbs } => {
                self.assets.thumb_workers = self.assets.thumb_workers.saturating_sub(1);
                if self.assets.root.as_deref() != Some(&root) {
                    return;
                }
                let frame = self.assets.frame;
                for (id, image) in thumbs {
                    self.assets.thumbs.insert(id, (image.map_or(Thumb::Missing, Thumb::Ready), frame));
                }
                self.assets.evict();
            }
            Done::AssetPreview { root, id, image }
                if self.assets.root.as_deref() == Some(&root) && self.assets.preview.as_ref().is_some_and(|(p, _)| *p == id) =>
            {
                self.assets.preview = Some((id, image.map_or(Thumb::Missing, Thumb::Ready)));
            }
            _ => {}
        }
    }

    /// project_inventory_scan finished.
    pub(crate) fn assets_scanned(&mut self, result: Result<Value, ToolError>) {
        self.assets.scanning = None;
        match result {
            Ok(v) => {
                let n = v["counts"]["items"].as_u64().unwrap_or(0);
                self.toast(trf("Scanned: {n} assets.", &[("n", &count(n))]), false);
                if let Some(root) = self.workspace.as_ref().map(|w| w.root.clone()) {
                    self.assets.summary = None;
                    self.assets.count_error = None;
                    self.count_assets(&root);
                    if self.assets.root.as_ref() == Some(&root) {
                        self.load_assets();
                    }
                }
            }
            Err(e) if craft_anything_engine::clean_room::is_refusal(&e) => self.state.modal = Some(Modal::DeepMode),
            Err(e) => self.toast(e.for_people(), true),
        }
    }
}

/// A filter chip: the label, picked or not.
fn chip(ui: &mut Ui, label: &str, on: bool) -> egui::Response {
    chip_named(ui, label, label, on)
}

/// [`chip`] with its own name for screen readers.
fn chip_named(ui: &mut Ui, label: &str, name: &str, on: bool) -> egui::Response {
    let t = Tokens::get(ui.ctx());
    let g = widgets::label_galley(ui, label, 12.0, t.text);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(g.size().x + 16.0, 24.0), Sense::click());
    let fill = if on { t.accent_soft } else { theme::mix(t.raised, t.hover, widgets::hover_t(ui, &resp)) };
    ui.painter().rect_filled(rect, t.r_sm(), fill);
    ui.painter().galley_with_override_text_color(rect.center() - g.size() / 2.0, g, if on { t.accent_ink } else { t.text_dim });
    widgets::focus_ring(ui, &resp, rect);
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, on, name));
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// The Assets rail: what's listed, the scan, and the filters.
pub fn rail(app: &mut CraftApp, ui: &mut Ui) {
    let t = Tokens::get(ui.ctx());
    ui.label(theme::label_text(&tr("Assets"), 12.0).color(t.text));
    ui.add_space(6.0);
    let Some(ws) = app.workspace.clone() else {
        widgets::empty_state(ui, "image", "No project open", "Open a project to see its pictures, models and sounds.");
        return;
    };
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        scan_box(app, ui);
        ui.add_space(10.0);
        if app.assets.root.as_ref() != Some(&ws.root) || app.assets.items.is_empty() {
            return;
        }
        widgets::named(
            ui.add(
                egui::TextEdit::singleline(&mut app.assets.query)
                    .hint_text(tr("Filter…"))
                    .desired_width(f32::INFINITY)
                    .frame(widgets::text_field_frame_compact(&t)),
            ),
            "Filter",
        );
        ui.add_space(8.0);
        crate::asset_plan::category_list(app, ui);
        ui.add_space(8.0);
        widgets::caption(ui, "Source");
        let sel = match app.assets.source {
            None => 0,
            Some(Source::Original) => 1,
            Some(Source::Build) => 2,
        };
        if let Some(i) = widgets::segmented(ui, &["All", "Original", "Build"], sel, 26.0) {
            app.assets.source = [None, Some(Source::Original), Some(Source::Build)].get(i).copied().flatten();
        }
        ui.add_space(8.0);
        widgets::caption(ui, "Kind");
        let all: usize = Kind::ALL.iter().map(|k| app.assets.kind_count(*k)).sum();
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::splat(4.0);
            if chip(ui, &format!("{} {}", tr("All"), count(all as u64)), app.assets.kind.is_none()).clicked() {
                app.assets.kind = None;
            }
            for k in Kind::ALL {
                let n = app.assets.kind_count(k);
                if n == 0 && app.assets.kind != Some(k) {
                    continue;
                }
                if chip(ui, &format!("{} {}", tr(kind_words(k)), count(n as u64)), app.assets.kind == Some(k)).clicked() {
                    app.assets.kind = if app.assets.kind == Some(k) { None } else { Some(k) };
                }
            }
        });
        ui.add_space(8.0);
        widgets::caption(ui, "Status");
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::splat(4.0);
            if chip(ui, &tr("Any status"), app.assets.status.is_none()).clicked() {
                app.assets.status = None;
            }
            for s in STATUSES {
                if chip(ui, &tr(status_words(s)), app.assets.status == Some(s)).clicked() {
                    app.assets.status = if app.assets.status == Some(s) { None } else { Some(s) };
                }
            }
        });
        ui.add_space(8.0);
        let n = app.assets.unreadable_count();
        let mut hide = !app.assets.show_unreadable;
        widgets::toggle(ui, &mut hide, &trf("Hide unreadable ({n})", &[("n", &count(n as u64))])).on_hover_text(tr(
            "Assets the scan couldn't take out of the original: no file and no picture, only why (a texture the game fills while it runs, say).",
        ));
        app.assets.show_unreadable = !hide;
    });
}

/// The scan button and what it says about the original.
fn scan_box(app: &mut CraftApp, ui: &mut Ui) {
    let t = Tokens::get(ui.ctx());
    if let Some(job) = app.assets.scanning {
        let p = app.jobs.list.iter().find(|j| j.id == job).and_then(|j| j.progress());
        widgets::working(ui, "Scanning the assets…", p.as_ref().map(|p| p.message.as_str()));
        return;
    }
    let scanned = app.assets.summary.as_ref().is_some_and(|s| s.scanned);
    if widgets::secondary(ui, Some("refresh-cw"), if scanned { "Scan again" } else { "Scan the assets" })
        .on_hover_text(tr("Lists every picture, model and sound in what you're making (and in the original), so you can see what to replace."))
        .clicked()
    {
        app.scan_assets();
    }
    if app.view.clean_room {
        ui.add_space(4.0);
        ui.add(
            egui::Label::new(
                RichText::new(tr("Clean room scans your build only. The original's assets need Deep mode.")).color(t.text_faint).size(theme::CAPTION),
            )
            .wrap(),
        );
        if widgets::link(ui, "Turn on Deep mode").clicked() {
            app.state.modal = Some(Modal::DeepMode);
        }
    }
}

/// The grid of the open project's assets (the Workbench's centre while the Assets rail is on).
pub fn center(app: &mut CraftApp, ui: &mut Ui) {
    let t = Tokens::get(ui.ctx());
    let Some(ws) = app.workspace.clone() else {
        ui.add_space(ui.available_height() * 0.2);
        widgets::empty_state(ui, "image", "No project open", "Open a project to see its pictures, models and sounds.");
        return;
    };
    app.assets.reset_for(Some(&ws.root));
    app.assets.frame += 1;
    match app.assets.list.clone() {
        ListState::Idle => {
            app.load_assets();
            return;
        }
        ListState::Loading => {
            ui.horizontal(|ui| {
                ui.add(widgets::Spinner::new().color(t.accent));
                ui.label(RichText::new(tr("Reading the asset list…")).color(t.text_dim));
            });
            return;
        }
        ListState::Failed(msg) => {
            widgets::empty_state(ui, "triangle-alert", "Couldn't read the asset list", &msg);
            ui.vertical_centered(|ui| {
                if widgets::secondary(ui, Some("refresh-cw"), "Try again").clicked() {
                    app.load_assets();
                }
            });
            return;
        }
        ListState::Ready => {}
    }
    if app.assets.items.is_empty() {
        ui.add_space(ui.available_height() * 0.15);
        let hidden = app.assets.summary.as_ref().map_or(0, |s| s.hidden);
        if hidden > 0 {
            widgets::empty_state(
                ui,
                "lock",
                "The original's assets need Deep mode",
                "Clean room never looks inside the original. Turn on Deep mode to see them.",
            );
        } else {
            widgets::empty_state(ui, "image", "No assets listed yet", "Scan the project to list its pictures, models and sounds.");
        }
        ui.vertical_centered(|ui| scan_box(app, ui));
        return;
    }
    toolbar(app, ui);
    ui.add_space(6.0);
    grid(app, ui);
}

/// Above the grid: how many are shown and selected, and marking the selection.
fn toolbar(app: &mut CraftApp, ui: &mut Ui) {
    let t = Tokens::get(ui.ctx());
    let shown = app.assets.shown().len();
    let picked = app.assets.selected.len();
    let mut mark = None;
    if let Some(c) = app.assets.category {
        crate::asset_plan::category_header(app, ui, c, shown);
    }
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(trf("{n} shown", &[("n", &count(shown as u64))])).color(t.text_dim));
        // One item is marked from the Inspector; several from here too.
        if picked > 1 {
            ui.label(RichText::new("·").color(t.text_faint));
            ui.label(RichText::new(trf("{n} selected", &[("n", &count(picked as u64))])).color(t.text));
            for m in MarkAs::ALL {
                if widgets::secondary(ui, None, m.words()).clicked() {
                    mark = Some(m);
                }
            }
            if widgets::link(ui, "Clear selection").clicked() {
                app.assets.selected.clear();
            }
            crate::asset_plan::selection_controls(app, ui, "toolbar");
        }
        if app.assets.marking > 0 {
            ui.add(widgets::Spinner::new().size(12.0).color(t.accent));
        }
    });
    if let Some(m) = mark {
        let ids: Vec<String> = app.assets.selected.iter().cloned().collect();
        app.mark_assets(&ids, m, None);
    }
}

/// The tiles, a row at a time, drawn only for the rows in view.
fn grid(app: &mut CraftApp, ui: &mut Ui) {
    let t = Tokens::get(ui.ctx());
    let shown: Vec<usize> = app.assets.shown().to_vec();
    if shown.is_empty() {
        widgets::empty_state(ui, "search", "No matches", "");
        return;
    }
    let cols = (((ui.available_width() + GAP) / (TILE_W + GAP)).floor() as usize).max(1);
    let rows = shown.len().div_ceil(cols);
    let mut want: Vec<(String, String)> = Vec::new();
    let mut drawn: Vec<String> = Vec::new();
    // A press picks a tile (as in a file manager); a click without a press (the keyboard, a screen
    // reader) picks it too. Each tile's id comes from its item, so it is the same widget from the
    // press to the release whatever row it is drawn in.
    let mut picked: Option<(usize, egui::Modifiers, bool)> = None;
    if !ui.input(|i| i.pointer.any_down() || i.pointer.any_released()) {
        app.assets.pressed = None;
    }
    let g = &app.assets;
    // The rows' spacing is set before show_rows measures them, so the rows it counts are the rows drawn.
    ui.spacing_mut().item_spacing = Vec2::splat(GAP);
    // Another category, kind, status or search starts at the top, not where the last list was.
    let filters: GridFilters = (g.category, g.source, g.kind, g.status, g.query.clone(), g.show_unreadable);
    let mut area = egui::ScrollArea::vertical().id_salt("asset-grid").auto_shrink([false, false]);
    if g.scrolled_for.as_ref() != Some(&filters) {
        area = area.vertical_scroll_offset(0.0);
    }
    area.show_rows(ui, TILE_H, rows, |ui, range| {
        for r in range {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = GAP;
                for c in 0..cols {
                    let pos = r * cols + c;
                    let Some(item) = shown.get(pos).and_then(|i| g.items.get(*i)) else { break };
                    let thumb = g.thumbs.get(&item.id).map(|(th, _)| th);
                    if let Some(rel) = &item.thumb {
                        if thumb.is_none() {
                            want.push((item.id.clone(), rel.clone()));
                        } else {
                            drawn.push(item.id.clone());
                        }
                    }
                    let selected = g.selected.contains(&item.id);
                    let focused = g.focus.as_deref() == Some(item.id.as_str());
                    let resp = tile(ui, item, thumb, selected, focused, &t);
                    let mods = ui.input(|i| i.modifiers);
                    if resp.is_pointer_button_down_on() && ui.input(|i| i.pointer.primary_pressed()) {
                        picked = Some((pos, mods, true));
                    } else if resp.clicked() {
                        picked = Some((pos, mods, false));
                    }
                }
            });
        }
    });
    app.assets.scrolled_for = Some(filters);
    let frame = app.assets.frame;
    for id in drawn {
        if let Some((_, at)) = app.assets.thumbs.get_mut(&id) {
            *at = frame;
        }
    }
    let ctx = ui.ctx().clone();
    app.want_asset_thumbs(&ctx, want);
    if let Some((pos, mods, press)) = picked {
        let id = shown.get(pos).and_then(|i| app.assets.items.get(*i)).map(|i| i.id.clone());
        if press {
            app.assets.pressed = id;
            select(app, &shown, pos, mods);
        } else if app.assets.pressed.is_some() && app.assets.pressed.take() == id {
            // The release of the press that already picked it.
        } else {
            select(app, &shown, pos, mods);
        }
    }
}

/// A click on the tile at `pos`: alone, added (Ctrl/⌘) or a range from the last click (Shift).
fn select(app: &mut CraftApp, shown: &[usize], pos: usize, mods: egui::Modifiers) {
    let g = &mut app.assets;
    let Some(id) = shown.get(pos).and_then(|i| g.items.get(*i)).map(|i| i.id.clone()) else { return };
    if mods.shift
        && let Some(from) = g.anchor.as_ref().and_then(|a| shown.iter().position(|i| g.items.get(*i).is_some_and(|it| &it.id == a)))
    {
        let (lo, hi) = (from.min(pos), from.max(pos));
        if !mods.command {
            g.selected.clear();
        }
        for i in shown.get(lo..=hi).unwrap_or_default() {
            if let Some(it) = g.items.get(*i) {
                g.selected.insert(it.id.clone());
            }
        }
    } else if mods.command {
        if !g.selected.remove(&id) {
            g.selected.insert(id.clone());
        }
        g.anchor = Some(id.clone());
    } else {
        g.selected.clear();
        g.selected.insert(id.clone());
        g.anchor = Some(id.clone());
    }
    g.focus = Some(id);
    app.state.right = RightTab::Inspector;
}

/// One tile: the thumbnail (or the kind's icon), the name, and its size or length.
fn tile(ui: &mut Ui, item: &Item, thumb: Option<&Thumb>, selected: bool, focused: bool, t: &Tokens) -> egui::Response {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(TILE_W, TILE_H), Sense::hover());
    let resp = ui.interact(rect, egui::Id::new(("asset-tile", item.id.as_str())), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, &item.name));
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let fill = if selected { t.accent_soft } else { theme::mix(t.card, t.hover, widgets::hover_t(ui, &resp)) };
    let p = ui.painter();
    p.rect(rect, t.radius, fill, egui::Stroke::new(1.0, if focused { t.accent } else { t.card_border }), egui::StrokeKind::Inside);
    let pic = egui::Rect::from_min_size(rect.min + Vec2::new(6.0, 6.0), Vec2::new(TILE_W - 12.0, TILE_H - 50.0));
    p.rect_filled(pic, t.radius_sm, t.raised);
    match thumb {
        Some(Thumb::Ready(tex)) => {
            let [w, h] = tex.size();
            let scale = (pic.width() / w.max(1) as f32).min(pic.height() / h.max(1) as f32);
            let fit = egui::Rect::from_center_size(pic.center(), Vec2::new(w as f32 * scale, h as f32 * scale));
            p.image(tex.id(), fit, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
        }
        Some(Thumb::Loading) => {}
        _ => icons::paint(ui, pic, kind_icon(item.kind), 30.0, t.text_faint),
    }
    let p = ui.painter();
    if let Some(col) = match item.status {
        Status::NeedsReplacing => Some(t.warning),
        Status::Replaced => Some(t.success),
        Status::Placeholder => Some(t.text_faint),
        _ => None,
    } {
        p.circle_filled(pic.right_top() + Vec2::new(-8.0, 8.0), 5.0, col);
        p.circle_stroke(pic.right_top() + Vec2::new(-8.0, 8.0), 5.0, egui::Stroke::new(1.5, t.card));
    }
    let clip = rect.shrink2(Vec2::new(6.0, 0.0));
    let name = p.layout_no_wrap(item.name.clone(), egui::FontId::proportional(theme::CAPTION), t.text);
    p.with_clip_rect(clip).galley(egui::pos2(rect.left() + 6.0, pic.bottom() + 6.0), name, t.text);
    let detail = p.layout_no_wrap(item_detail(item), egui::FontId::proportional(theme::CAPTION), t.text_faint);
    p.with_clip_rect(clip).galley(egui::pos2(rect.left() + 6.0, pic.bottom() + 24.0), detail, t.text_faint);
    resp.on_hover_text(item.path.as_deref().or(item.origin.as_deref()).unwrap_or(&item.name))
}

/// "512×512 · 35 KB", "2.2 s · 112 KB" or the size.
fn item_detail(item: &Item) -> String {
    let mut parts = Vec::new();
    if let (Some(w), Some(h)) = (item.width, item.height) {
        parts.push(format!("{w}×{h}"));
    }
    if let Some(d) = item.duration_secs.filter(|d| d.is_finite() && *d >= 0.0) {
        parts.push(seconds(d));
    }
    if item.bytes > 0 {
        parts.push(crate::human_bytes(usize::try_from(item.bytes).unwrap_or(usize::MAX)));
    }
    parts.join(" · ")
}

fn seconds(d: f64) -> String {
    if d < 60.0 { format!("{d:.1} s") } else { format!("{}:{:02}", (d / 60.0).floor() as u64, (d % 60.0).floor() as u64) }
}

fn kv(ui: &mut Ui, k: &str, v: &str, t: &Tokens) {
    ui.label(RichText::new(tr(k)).color(t.text_faint).size(theme::CAPTION));
    ui.add(egui::Label::new(RichText::new(v).font(theme::mono(12.0))).wrap());
    ui.end_row();
}

/// The Inspector while the Assets rail is on: the focused item, large, with its details, status
/// buttons, note and file location; or the selection, to mark together.
pub fn inspector(app: &mut CraftApp, ui: &mut Ui) {
    let t = Tokens::get(ui.ctx());
    let item = app.assets.focus.as_deref().and_then(|id| app.assets.item(id)).cloned();
    let Some(item) = item else {
        widgets::empty_state(ui, "image", "Pick an asset", "Click a tile to see it here. Ctrl/⌘-click or Shift-click picks several.");
        return;
    };
    if app.assets.note_for.as_deref() != Some(item.id.as_str()) {
        app.assets.note_for = Some(item.id.clone());
        app.assets.note = item.note.clone();
    }
    let ctx = ui.ctx().clone();
    app.want_asset_preview(&ctx, &item);
    let mut mark = None;
    let mut save_note = false;
    let mut reveal = None;
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        let picked = app.assets.selected.len();
        if picked > 1 {
            widgets::panel(&t).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new(trf("{n} selected", &[("n", &count(picked as u64))])).font(theme::semibold(theme::SMALL)));
                ui.horizontal_wrapped(|ui| {
                    for m in MarkAs::ALL {
                        if widgets::secondary(ui, None, m.words()).clicked() {
                            mark = Some((app.assets.selected.iter().cloned().collect::<Vec<_>>(), m));
                        }
                    }
                });
                crate::asset_plan::selection_controls(app, ui, "inspector");
            });
            ui.add_space(8.0);
        }
        widgets::panel(&t).show(ui, |ui| {
            ui.set_width(ui.available_width());
            let side = ui.available_width().min(300.0);
            let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), side), Sense::hover());
            ui.painter().rect_filled(rect, t.radius_sm, t.raised);
            let tex = match &app.assets.preview {
                Some((id, Thumb::Ready(tex))) if *id == item.id => Some(tex.clone()),
                _ => app.assets.thumbs.get(&item.id).and_then(|(th, _)| if let Thumb::Ready(tex) = th { Some(tex.clone()) } else { None }),
            };
            match tex {
                Some(tex) => {
                    let [w, h] = tex.size();
                    let scale = (rect.width() / w.max(1) as f32).min(rect.height() / h.max(1) as f32).min(4.0);
                    let fit = egui::Rect::from_center_size(rect.center(), Vec2::new(w as f32 * scale, h as f32 * scale));
                    ui.painter().image(tex.id(), fit, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
                }
                None => icons::paint(ui, rect, kind_icon(item.kind), 48.0, t.text_faint),
            }
            ui.add_space(6.0);
            ui.add(egui::Label::new(RichText::new(&item.name).font(theme::semibold(theme::BODY))).wrap());
            ui.add_space(4.0);
            egui::Grid::new("asset-grid-kv").num_columns(2).spacing([10.0, 4.0]).show(ui, |ui| {
                kv(ui, "Kind", &tr(kind_words(item.kind)), &t);
                kv(ui, "Source", &tr(if item.source == Source::Original { "Original" } else { "Build" }), &t);
                kv(ui, "Status", &tr(status_words(item.status)), &t);
                if let Some(p) = &item.path {
                    kv(ui, "Path", p, &t);
                }
                if item.bytes > 0 {
                    kv(ui, "Size", &crate::human_bytes(usize::try_from(item.bytes).unwrap_or(usize::MAX)), &t);
                }
                if let (Some(w), Some(h)) = (item.width, item.height) {
                    kv(ui, "Picture size", &format!("{w}×{h}"), &t);
                }
                if let Some(d) = item.duration_secs.filter(|d| d.is_finite() && *d >= 0.0) {
                    kv(ui, "Length", &seconds(d), &t);
                }
                if let Some(o) = &item.origin {
                    kv(ui, "Origin", o, &t);
                }
                if let Some(d) = &item.detail {
                    kv(ui, "Details", d, &t);
                }
            });
            if let Some(problem) = &item.problem {
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    icons::show(ui, "triangle-alert", 14.0, t.warning);
                    ui.add(egui::Label::new(RichText::new(problem).color(t.text_dim).size(theme::CAPTION)).wrap());
                });
            }
            ui.add_space(8.0);
            crate::asset_plan::item_category(app, ui, &item);
            ui.add_space(8.0);
            widgets::caption(ui, "Mark as");
            ui.horizontal_wrapped(|ui| {
                for m in MarkAs::ALL {
                    let name = format!("{}: {}", tr("Mark as"), tr(m.words()));
                    if chip_named(ui, &tr(m.words()), &name, m.is(item.status)).clicked() {
                        mark = Some((vec![item.id.clone()], m));
                    }
                }
            });
            ui.add_space(8.0);
            widgets::caption(ui, "Note");
            widgets::named(
                ui.add(
                    egui::TextEdit::multiline(&mut app.assets.note)
                        .hint_text(tr("What to replace it with, or why…"))
                        .desired_rows(2)
                        .desired_width(f32::INFINITY)
                        .frame(widgets::text_field_frame_compact(&t)),
                ),
                "Note",
            );
            ui.horizontal_wrapped(|ui| {
                if app.assets.note.trim() != item.note.trim() && widgets::secondary(ui, None, "Save note").clicked() {
                    save_note = true;
                }
                if let Some(p) = item.path.as_deref().and_then(|p| app.assets.root.as_deref().and_then(|r| inside(r, p)))
                    && widgets::secondary(ui, Some("folder-open"), "Open file location").clicked()
                {
                    reveal = Some(p);
                }
            });
        });
    });
    if let Some((ids, m)) = mark {
        app.mark_assets(&ids, m, None);
    }
    if save_note {
        app.save_asset_note();
    }
    if let Some(p) = reveal {
        app.reveal_asset(p);
    }
}

/// The project page's Assets card (game projects, or any project with an inventory): how many of
/// each kind, how many are marked to replace, and the way to the gallery.
pub fn card(app: &mut CraftApp, ui: &mut Ui, ws: &craft_anything_engine::workspace::Workspace) {
    let game = craft_anything_engines::game::Builder::from_stack(&ws.manifest.stack).is_some();
    app.count_assets(&ws.root);
    let summary = app.assets.summary.clone().filter(|s| s.root == ws.root);
    let error = app.assets.count_error.as_ref().filter(|(r, _)| *r == ws.root).map(|(_, e)| e.clone());
    if !game && summary.as_ref().is_none_or(|s| s.items == 0 && s.hidden == 0) {
        return;
    }
    let t = Tokens::get(ui.ctx());
    widgets::card(&t).inner_margin(Margin::symmetric(16, 14)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        widgets::caption(ui, "Assets");
        ui.add_space(6.0);
        match &summary {
            None if error.is_some() => {
                ui.add(egui::Label::new(RichText::new(error.unwrap_or_default()).size(theme::CAPTION).color(t.warning)).wrap());
            }
            None => {
                ui.add(widgets::Spinner::new().size(14.0).color(t.accent));
            }
            Some(s) if !s.scanned && s.hidden == 0 => {
                ui.label(RichText::new(tr("Not scanned yet.")).size(theme::SMALL).color(t.text_dim));
                ui.add_space(6.0);
                if app.assets.scanning.is_some() {
                    widgets::working(ui, "Scanning the assets…", None);
                } else if widgets::secondary(ui, Some("refresh-cw"), "Scan the assets").clicked() {
                    app.scan_assets();
                }
            }
            Some(s) => {
                let line = s.line();
                if !line.is_empty() {
                    ui.add(egui::Label::new(RichText::new(line).size(theme::SMALL).color(t.text)).wrap());
                }
                if s.marked > 0 {
                    ui.label(RichText::new(trf("{n} marked to replace", &[("n", &count(s.marked))])).size(theme::SMALL).color(t.warning));
                }
                crate::asset_plan::card_categories(app, ui, s);
                if s.hidden > 0 {
                    ui.add(
                        egui::Label::new(
                            RichText::new(trf("{n} of the original's assets need Deep mode to show.", &[("n", &count(s.hidden))]))
                                .size(theme::CAPTION)
                                .color(t.text_faint),
                        )
                        .wrap(),
                    );
                }
                ui.add_space(6.0);
                // In Simple mode it switches to Advanced, where the gallery is.
                if widgets::secondary(ui, Some("image"), "Open the gallery").clicked() {
                    if app.beginner() {
                        app.set_mode(craft_anything_engine::Mode::Experienced);
                    }
                    app.open_asset_gallery();
                }
            }
        }
    });
    ui.add_space(14.0);
}

/// Shared for tests: the gallery's thumbnails in memory now.
pub fn cached_thumbs(app: &CraftApp) -> usize {
    app.assets.thumbs.values().filter(|(t, _)| matches!(t, Thumb::Ready(_))).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, source: Source, kind: Kind, name: &str) -> Item {
        let mut i = Item::new(source, id, kind, name, if source == Source::Original { Status::Original } else { Status::Placeholder });
        i.id = id.into();
        i
    }

    #[test]
    fn filters_count_kinds_and_pass_matching_items() {
        let mut g = Gallery {
            items: vec![
                item("a", Source::Original, Kind::Texture, "Rock"),
                item("b", Source::Original, Kind::Sound, "Jump"),
                item("c", Source::Build, Kind::Texture, "rock_new"),
            ],
            ..Default::default()
        };
        assert_eq!(g.shown().len(), 3);
        assert_eq!(g.kind_count(Kind::Texture), 2);
        g.kind = Some(Kind::Texture);
        assert_eq!(g.shown(), &[0, 2]);
        g.source = Some(Source::Build);
        assert_eq!(g.shown(), &[2]);
        assert_eq!(g.kind_count(Kind::Sound), 0);
        g.source = None;
        g.kind = None;
        g.query = "ROCK".into();
        assert_eq!(g.shown(), &[0, 2]);
        g.apply(vec![Item { status: Status::NeedsReplacing, ..item("a", Source::Original, Kind::Texture, "Rock") }]);
        g.status = Some(Status::NeedsReplacing);
        assert_eq!(g.shown(), &[0]);
    }

    #[test]
    fn pictures_first_problems_last_and_unreadable_hidden_by_default() {
        let mut font = item("f", Source::Original, Kind::Texture, "Font Texture");
        font.problem = Some("an empty texture".into());
        let mut pic = item("p", Source::Original, Kind::Texture, "zebra");
        pic.thumb = Some("reference/thumbs/p.png".into());
        let mut broken_sound = item("b", Source::Original, Kind::Sound, "Boom");
        broken_sound.path = Some("reference/assets/sound/boom.fsb".into());
        broken_sound.problem = Some("an odd codec".into());
        let mut g = Gallery {
            items: vec![font, item("m", Source::Original, Kind::Model, "Crate"), broken_sound, pic, item("t", Source::Original, Kind::Texture, "Atlas")],
            ..Default::default()
        };
        // zebra (a picture), Atlas (a texture without one), Crate, then problems: Boom (it has a file).
        assert_eq!(g.shown(), &[3, 4, 1, 2]);
        assert_eq!(g.unreadable_count(), 1);
        assert_eq!(g.kind_count(Kind::Texture), 2);
        g.show_unreadable = true;
        assert_eq!(g.shown(), &[3, 4, 1, 0, 2], "problems last, by kind");
        assert_eq!(g.kind_count(Kind::Texture), 3);
        g.kind = Some(Kind::Sound);
        assert_eq!(g.unreadable_count(), 0, "counted under the current filter");
    }

    #[test]
    fn keep_keeps_what_each_item_is() {
        let original = item("a", Source::Original, Kind::Texture, "Rock");
        let placeholder = item("b", Source::Build, Kind::Texture, "Rock");
        let generated = Item { status: Status::Generated, ..item("c", Source::Build, Kind::Model, "Crate") };
        assert_eq!(MarkAs::Keep.status_for(&original), Status::Original);
        assert_eq!(MarkAs::Keep.status_for(&placeholder), Status::Library);
        assert_eq!(MarkAs::Keep.status_for(&generated), Status::Generated);
        assert_eq!(MarkAs::NeedsReplacing.status_for(&original), Status::NeedsReplacing);
    }

    #[test]
    fn counts_and_paths_are_safe() {
        assert_eq!(count(0), "0");
        assert_eq!(count(1932), "1,932");
        assert_eq!(count(1_234_567), "1,234,567");
        let root = Path::new("/p");
        assert!(inside(root, "reference/thumbs/a.png").is_some());
        assert!(inside(root, "../secret.png").is_none());
        assert!(inside(root, "").is_none());
        assert_eq!(seconds(2.159), "2.2 s");
        assert_eq!(seconds(125.0), "2:05");
    }

    #[test]
    fn a_summary_reads_the_list_tools_counts() {
        let v = json!({"build_scanned_ms": null, "original_scanned_ms": 5, "counts": {"items": 3, "by_kind": {"original": {"texture": 2, "sound": 1}}, "by_status": {"needs_replacing": 1}}});
        let s = Summary::from_list(Path::new("/p"), &v);
        assert!(s.scanned);
        assert_eq!(s.marked, 1);
        assert_eq!(s.by_kind.get(&Kind::Texture), Some(&2));
        assert_eq!(s.line(), "Textures 2 · Sounds 1");
    }
}
