//! A project's asset inventory (`inventory.json`): every picture, model, sound, font, video and
//! engine asset in the build (app/), and what was taken out of the original into
//! `reference/assets/`, so the person can see what there is and mark what to replace.
//!
//! The file is versioned ([`INVENTORY_VERSION`], migrated by [`crate::versioned`]) and its schema
//! is published in `docs/schemas/inventory.schema.json` (`cargo xtask schemas`). A rescan keeps the
//! status and note the person gave each item that is still there ([`Item::marked_ms`]).
//!
//! What counts as an asset is data: [`EXTENSIONS`] maps a file's extension to its [`Kind`],
//! [`UNREAL_RULES`] sorts Unreal packages by folder or name prefix, and [`SKIPPED_FOLDERS`] lists
//! the build-output folders a scan leaves out.

use crate::project::write_atomic;
use crate::tool::CancelToken;
use crate::tools::web_common::{self, Image};
use crate::versioned::{self, upgrade};
use crate::workspace::{OWN_NAMES, Workspace};
use craft_anything_evidence::{ErrorCode, ToolError};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// The inventory file, in the project folder.
pub const INVENTORY: &str = "inventory.json";
/// The current `inventory.json` format version. Bumping it means appending a step to [`STEPS`].
pub const INVENTORY_VERSION: u32 = 1;
/// The steps that bring an older inventory forward; step `i` goes from version `i` to `i + 1`.
const STEPS: &[versioned::Step<Value>] = &[inventory_v0_to_v1];
const _: () = assert!(STEPS.len() == INVENTORY_VERSION as usize, "one step per inventory.json version");

/// Version 0 (no `version` field) has the shape of version 1; only the number is new.
fn inventory_v0_to_v1(doc: &mut Value) -> Result<(), String> {
    if doc.is_object() { Ok(()) } else { Err("the inventory isn't a JSON object".into()) }
}

/// The largest inventory file read (about 100 000 items).
const MAX_FILE: u64 = 64 << 20;
/// The most items one source holds.
pub const MAX_ITEMS: usize = 20_000;
/// Files and folders a build scan looks at, at most.
const MAX_ENTRIES: usize = 200_000;
/// How deep a build scan goes.
const MAX_DEPTH: usize = 32;
/// Thumbnails are at most this many pixels on their longer side.
pub const THUMB_SIDE: u32 = 256;
/// Pixels decoded for thumbnails in one scan, all pictures together (a picture over the rest of
/// the budget gets none).
pub const THUMB_PIXEL_BUDGET: u64 = 1 << 31;
/// The largest picture file decoded for a thumbnail.
const MAX_THUMB_FILE: u64 = 64 << 20;

/// Where an item is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// A file of the build (app/).
    Build,
    /// Taken out of the original (Deep mode only), into reference/assets/.
    Original,
}

impl Source {
    pub fn id(self) -> &'static str {
        match self {
            Source::Build => "build",
            Source::Original => "original",
        }
    }
}

/// What an item is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Texture,
    Sprite,
    Model,
    Sound,
    Music,
    Font,
    Video,
    Map,
    Material,
    EngineAsset,
    Other,
}

impl Kind {
    pub const ALL: [Kind; 11] = [
        Kind::Texture,
        Kind::Sprite,
        Kind::Model,
        Kind::Sound,
        Kind::Music,
        Kind::Font,
        Kind::Video,
        Kind::Map,
        Kind::Material,
        Kind::EngineAsset,
        Kind::Other,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Kind::Texture => "texture",
            Kind::Sprite => "sprite",
            Kind::Model => "model",
            Kind::Sound => "sound",
            Kind::Music => "music",
            Kind::Font => "font",
            Kind::Video => "video",
            Kind::Map => "map",
            Kind::Material => "material",
            Kind::EngineAsset => "engine_asset",
            Kind::Other => "other",
        }
    }
}

/// Where an item stands, for the person.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// The original's own asset.
    Original,
    /// Made by an asset generator (listed in assets.json).
    Generated,
    /// From a library or pack (a marketplace or starter pack, an engine's own content).
    Library,
    /// A stand-in the builder made or took; the starting status of the build's files.
    Placeholder,
    /// The person wants it replaced.
    NeedsReplacing,
    /// The person replaced it.
    Replaced,
}

impl Status {
    pub fn id(self) -> &'static str {
        match self {
            Status::Original => "original",
            Status::Generated => "generated",
            Status::Library => "library",
            Status::Placeholder => "placeholder",
            Status::NeedsReplacing => "needs_replacing",
            Status::Replaced => "replaced",
        }
    }
}

/// One asset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Item {
    /// Stable: a hash of the source and the item's key (`path` for the build, `origin` for the
    /// original), so it survives rescans.
    pub id: String,
    pub source: Source,
    pub kind: Kind,
    /// What it's called (the file's name, or the asset's name inside the original).
    pub name: String,
    /// The file, relative to the project folder (`/`-separated). None for an item of the original
    /// that wasn't taken out (`problem` or `detail` says why).
    #[serde(default)]
    pub path: Option<String>,
    /// For the original: where it is in the original, relative to the original's folder
    /// (`<archive>[:<file inside>]#<object id>`, or a loose file's path).
    #[serde(default)]
    pub origin: Option<String>,
    /// Its size on disk (or inside the original, when it wasn't taken out).
    #[serde(default)]
    pub bytes: u64,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub duration_secs: Option<f64>,
    /// A picture of it at most 256 pixels on a side (a PNG under reference/thumbs/), relative to
    /// the project folder.
    #[serde(default)]
    pub thumb: Option<String>,
    pub status: Status,
    /// The person's note.
    #[serde(default)]
    pub note: String,
    /// What it is in more words (an Unreal package's class, a sound bank's codec, what a generated
    /// asset was made from).
    #[serde(default)]
    pub detail: Option<String>,
    /// What went wrong reading or taking it out, when something did.
    #[serde(default)]
    pub problem: Option<String>,
    /// When the person last set its status or note (ms since the epoch); a rescan keeps a marked
    /// item's status and note.
    #[serde(default)]
    pub marked_ms: Option<u64>,
}

impl Item {
    /// A new item; its id comes from `source` and `key`.
    pub fn new(source: Source, key: &str, kind: Kind, name: impl Into<String>, status: Status) -> Item {
        Item {
            id: item_id(source, key),
            source,
            kind,
            name: name.into(),
            path: None,
            origin: None,
            bytes: 0,
            width: None,
            height: None,
            duration_secs: None,
            thumb: None,
            status,
            note: String::new(),
            detail: None,
            problem: None,
            marked_ms: None,
        }
    }
}

/// The stable id of the item `key` (a path or an origin) of `source`.
pub fn item_id(source: Source, key: &str) -> String {
    let hash = craft_anything_evidence::sha256_hex(format!("{}\0{key}", source.id()).as_bytes());
    hash.get(..16).unwrap_or(&hash).to_string()
}

/// The inventory file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Inventory {
    /// The format version ([`INVENTORY_VERSION`]).
    #[serde(default)]
    pub version: u32,
    /// When the build was last scanned (ms since the epoch).
    #[serde(default)]
    pub build_scanned_ms: Option<u64>,
    /// When the original was last scanned.
    #[serde(default)]
    pub original_scanned_ms: Option<u64>,
    /// The original's folder at that scan.
    #[serde(default)]
    pub original_folder: Option<String>,
    /// What the last scan of each source left out or couldn't do, in words.
    #[serde(default)]
    pub notes: BTreeMap<Source, Vec<String>>,
    #[serde(default)]
    pub items: Vec<Item>,
}

impl Inventory {
    /// Made by a newer Craft Anything: it is read as far as its fields are known, never written.
    pub fn is_newer(&self) -> bool {
        self.version > INVENTORY_VERSION
    }

    /// The items of `source`.
    pub fn of(&self, source: Source) -> impl Iterator<Item = &Item> {
        self.items.iter().filter(move |i| i.source == source)
    }

    /// Replace the items of `source` with `fresh`, keeping the person's status and note on every
    /// item that is still there.
    pub fn replace(&mut self, source: Source, mut fresh: Vec<Item>) {
        let old: HashMap<String, Item> = self.items.iter().filter(|i| i.source == source).map(|i| (i.id.clone(), i.clone())).collect();
        for item in &mut fresh {
            if let Some(prev) = old.get(&item.id) {
                keep_marks(item, prev);
            }
        }
        self.items.retain(|i| i.source != source);
        self.items.extend(fresh);
        self.items
            .sort_by(|a, b| (a.source, a.kind, a.path.as_deref().or(a.origin.as_deref())).cmp(&(b.source, b.kind, b.path.as_deref().or(b.origin.as_deref()))));
    }

    /// Bring the person's marks from `disk` (the file as it is now) onto these items: a mark made
    /// while a scan ran isn't lost when the scan saves.
    pub fn keep_newer_marks(&mut self, disk: &Inventory) {
        let marked: HashMap<&str, &Item> = disk.items.iter().filter(|i| i.marked_ms.is_some()).map(|i| (i.id.as_str(), i)).collect();
        for item in &mut self.items {
            if let Some(prev) = marked.get(item.id.as_str())
                && prev.marked_ms > item.marked_ms
            {
                keep_marks(item, prev);
            }
        }
    }

    /// How many items there are of each source, kind and status.
    pub fn counts(&self) -> Value {
        let mut by_source: BTreeMap<&str, u64> = BTreeMap::new();
        let mut by_kind: BTreeMap<&str, BTreeMap<&str, u64>> = BTreeMap::new();
        let mut by_status: BTreeMap<&str, u64> = BTreeMap::new();
        let (mut not_taken_out, mut problems) = (0u64, 0u64);
        for i in &self.items {
            *by_source.entry(i.source.id()).or_default() += 1;
            *by_kind.entry(i.source.id()).or_default().entry(i.kind.id()).or_default() += 1;
            *by_status.entry(i.status.id()).or_default() += 1;
            not_taken_out += u64::from(i.path.is_none());
            problems += u64::from(i.problem.is_some());
        }
        serde_json::json!({
            "items": self.items.len(),
            "by_source": by_source,
            "by_kind": by_kind,
            "by_status": by_status,
            "without_file": not_taken_out,
            "with_problem": problems,
        })
    }
}

fn keep_marks(item: &mut Item, prev: &Item) {
    if prev.marked_ms.is_some() {
        item.status = prev.status;
        item.note = prev.note.clone();
        item.marked_ms = prev.marked_ms;
    }
}

/// Read an `inventory.json` document of any version this build understands. A newer one is read
/// as far as its fields are known and keeps its version, so [`save`] refuses to write it.
pub fn parse(bytes: &[u8]) -> Result<Inventory, ToolError> {
    let mut doc: Value = serde_json::from_slice(bytes).map_err(|e| ToolError::invalid(format!("{INVENTORY} isn't valid JSON: {e}")))?;
    let found = match doc.get("version") {
        None | Some(Value::Null) => 0,
        Some(v) => {
            v.as_u64().and_then(|n| u32::try_from(n).ok()).ok_or_else(|| ToolError::invalid(format!("{INVENTORY}'s version ({v}) isn't a whole number")))?
        }
    };
    if found <= INVENTORY_VERSION {
        let reached = upgrade(&mut doc, found, 0, STEPS).map_err(|e| ToolError::invalid(format!("{INVENTORY} couldn't be brought up to date: {e}")))?;
        if let Some(obj) = doc.as_object_mut() {
            obj.insert("version".into(), Value::from(reached));
        }
    }
    serde_json::from_value(doc).map_err(|e| ToolError::invalid(format!("{INVENTORY} can't be read: {e}")))
}

/// The inventory of the project at `root`; none yet is an empty one.
pub fn load(root: &Path) -> Result<Inventory, ToolError> {
    let path = root.join(INVENTORY);
    let len = match std::fs::metadata(&path) {
        Ok(m) => m.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Inventory { version: INVENTORY_VERSION, ..Inventory::default() }),
        Err(e) => return Err(ToolError::io(path.display(), e)),
    };
    if len > MAX_FILE {
        return Err(ToolError::new(ErrorCode::OverLimit, format!("{} is {len} bytes; the limit is {MAX_FILE}.", path.display()))
            .with_remediation("Delete it and scan again (the marks in it are lost)."));
    }
    let bytes = std::fs::read(&path).map_err(|e| ToolError::io(path.display(), e))?;
    parse(&bytes)
}

/// Save the inventory of the project at `root`. A newer file on disk is never written over.
pub fn save(root: &Path, inv: &Inventory) -> Result<(), ToolError> {
    if inv.is_newer() {
        return Err(newer(inv.version));
    }
    if let Ok(disk) = load(root)
        && disk.is_newer()
    {
        return Err(newer(disk.version));
    }
    let mut inv = inv.clone();
    inv.version = INVENTORY_VERSION;
    let bytes = serde_json::to_vec_pretty(&inv).map_err(|e| ToolError::internal(e.to_string()))?;
    write_atomic(&root.join(INVENTORY), &bytes)
}

fn newer(version: u32) -> ToolError {
    ToolError::unsupported(format!(
        "{INVENTORY} was written by a newer Craft Anything (version {version}; this one reads up to {INVENTORY_VERSION}), so it isn't changed."
    ))
    .with_remediation("Update Craft Anything to change it. Nothing was written.")
}

/// The JSON Schema of `inventory.json` as published in `docs/schemas/inventory.schema.json`.
pub fn inventory_schema() -> String {
    let schema = schemars::generate::SchemaSettings::draft2020_12().into_generator().into_root_schema_for::<Inventory>();
    serde_json::to_string_pretty(&schema).map(|s| s + "\n").unwrap_or_default()
}

// ---- What counts as an asset ----

/// File extensions (lower case) and the kind of asset they are.
pub const EXTENSIONS: &[(&str, Kind)] = &[
    // Pictures.
    ("png", Kind::Texture),
    ("jpg", Kind::Texture),
    ("jpeg", Kind::Texture),
    ("tga", Kind::Texture),
    ("bmp", Kind::Texture),
    ("gif", Kind::Texture),
    ("webp", Kind::Texture),
    ("exr", Kind::Texture),
    ("hdr", Kind::Texture),
    ("psd", Kind::Texture),
    ("dds", Kind::Texture),
    ("ktx", Kind::Texture),
    ("ktx2", Kind::Texture),
    ("svg", Kind::Texture),
    // Models.
    ("fbx", Kind::Model),
    ("glb", Kind::Model),
    ("gltf", Kind::Model),
    ("obj", Kind::Model),
    ("blend", Kind::Model),
    ("usd", Kind::Model),
    ("usda", Kind::Model),
    ("usdz", Kind::Model),
    ("dae", Kind::Model),
    ("3ds", Kind::Model),
    ("ply", Kind::Model),
    ("stl", Kind::Model),
    // Sounds.
    ("wav", Kind::Sound),
    ("ogg", Kind::Sound),
    ("mp3", Kind::Sound),
    ("flac", Kind::Sound),
    ("aif", Kind::Sound),
    ("aiff", Kind::Sound),
    ("opus", Kind::Sound),
    // Fonts.
    ("ttf", Kind::Font),
    ("otf", Kind::Font),
    ("woff", Kind::Font),
    ("woff2", Kind::Font),
    ("ufont", Kind::Font),
    // Video.
    ("mp4", Kind::Video),
    ("webm", Kind::Video),
    ("mov", Kind::Video),
    // Unreal packages (sorted further by `UNREAL_RULES`).
    ("uasset", Kind::EngineAsset),
    ("umap", Kind::Map),
    // Unity.
    ("prefab", Kind::EngineAsset),
    ("mat", Kind::Material),
    ("asset", Kind::EngineAsset),
    ("unity", Kind::Map),
    // Godot.
    ("tres", Kind::EngineAsset),
    ("tscn", Kind::EngineAsset),
];

/// How an Unreal package is sorted: by a folder on its path, or by its name's prefix (Unreal's
/// naming convention). The first rule that matches wins.
pub enum UnrealRule {
    /// A folder named this (any case) on the path.
    Folder(&'static str, Kind),
    /// The file name starts with this (case matters: `T_`, `SM_`).
    Prefix(&'static str, Kind),
}

pub const UNREAL_RULES: &[UnrealRule] = &[
    UnrealRule::Prefix("MI_", Kind::Material),
    UnrealRule::Prefix("M_", Kind::Material),
    UnrealRule::Prefix("T_", Kind::Texture),
    UnrealRule::Prefix("SM_", Kind::Model),
    UnrealRule::Prefix("SK_", Kind::Model),
    UnrealRule::Prefix("S_", Kind::Sound),
    UnrealRule::Folder("Maps", Kind::Map),
    UnrealRule::Folder("Textures", Kind::Texture),
    UnrealRule::Folder("Meshes", Kind::Model),
    UnrealRule::Folder("Audio", Kind::Sound),
    UnrealRule::Folder("Materials", Kind::Material),
];

/// Folders a build scan leaves out: what a build makes again, version control, screenshots.
pub const SKIPPED_FOLDERS: &[&str] =
    &["Binaries", "Intermediate", "Saved", "DerivedDataCache", "Build", "node_modules", "target", ".git", "Library", "Temp", ".godot", "shots"];

/// Path parts that mark a library or pack (the status of what's under them is `library`).
pub const LIBRARY_FOLDERS: &[&str] = &["StarterContent", "ThirdParty", "third_party", "vendor", "Megascans", "Marketplace", "Kenney", "AssetPacks"];

/// Words in a sound's path that make it music.
const MUSIC_WORDS: &[&str] = &["music", "bgm", "soundtrack", "theme", "mus_"];
/// A sound at least this long is music.
const MUSIC_SECS: f64 = 60.0;

/// What kind of asset the file at `rel` (a `/`-separated path) is, or None when it isn't one.
pub fn classify(rel: &str) -> Option<Kind> {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase())?;
    let kind = EXTENSIONS.iter().find(|(e, _)| *e == ext).map(|(_, k)| *k)?;
    Some(match kind {
        Kind::EngineAsset if ext == "uasset" => unreal_kind(rel),
        Kind::Texture if has_part(rel, "sprite") || has_part(rel, "sprites") => Kind::Sprite,
        Kind::Sound if is_music(rel, None) => Kind::Music,
        k => k,
    })
}

/// An Unreal package's kind, from its folder or name ([`UNREAL_RULES`]).
pub fn unreal_kind(rel: &str) -> Kind {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    for rule in UNREAL_RULES {
        match rule {
            UnrealRule::Prefix(p, k) if name.starts_with(p) => return *k,
            UnrealRule::Folder(f, k) if has_part(rel, f) => return *k,
            _ => {}
        }
    }
    Kind::EngineAsset
}

/// Whether a sound is music: its path says so, or it lasts a minute or more.
pub fn is_music(rel: &str, duration_secs: Option<f64>) -> bool {
    let lower = rel.to_lowercase();
    MUSIC_WORDS.iter().any(|w| lower.contains(w)) || duration_secs.is_some_and(|d| d >= MUSIC_SECS)
}

/// Whether a folder of `rel` is named `part` (any case).
fn has_part(rel: &str, part: &str) -> bool {
    let mut folders = rel.split('/').collect::<Vec<_>>();
    folders.pop();
    folders.iter().any(|f| f.eq_ignore_ascii_case(part))
}

fn skipped_folder(name: &str) -> bool {
    SKIPPED_FOLDERS.iter().any(|s| s.eq_ignore_ascii_case(name))
}

/// The status a build file starts with.
fn build_status(rel: &str) -> Status {
    if LIBRARY_FOLDERS.iter().any(|l| has_part(rel, l)) { Status::Library } else { Status::Placeholder }
}

/// `p` relative to `root`, `/`-separated; None when it isn't under it or isn't valid text.
pub fn relative(root: &Path, p: &Path) -> Option<String> {
    let rel = p.strip_prefix(root).ok()?;
    let parts: Option<Vec<&str>> = rel.components().map(|c| c.as_os_str().to_str()).collect();
    Some(parts?.join("/"))
}

// ---- Scanning the build ----

/// What a scan does with pictures: their thumbnails, within a budget of decoded pixels.
pub struct Thumbs {
    /// The project folder (thumbnails go to reference/thumbs/).
    root: PathBuf,
    /// Pixels still allowed.
    left: u64,
    pub made: u64,
    pub over_budget: u64,
}

impl Thumbs {
    pub fn new(root: &Path) -> Thumbs {
        Thumbs { root: root.to_path_buf(), left: THUMB_PIXEL_BUDGET, made: 0, over_budget: 0 }
    }

    fn rel(id: &str) -> String {
        format!("reference/thumbs/{id}.png")
    }

    /// The thumbnail of item `id` when it's already there and not older than `source`.
    pub fn existing(&self, id: &str, source: Option<&Path>) -> Option<String> {
        let rel = Thumbs::rel(id);
        let made = std::fs::metadata(self.root.join(&rel)).and_then(|m| m.modified()).ok()?;
        let changed = source.and_then(|s| std::fs::metadata(s).and_then(|m| m.modified()).ok());
        changed.is_none_or(|c| c <= made).then_some(rel)
    }

    /// Make item `id`'s thumbnail from a decoded picture. None (and nothing written) when it's
    /// over the pixel budget or can't be written.
    pub fn from_image(&mut self, id: &str, img: &Image) -> Option<String> {
        let pixels = u64::from(img.width).saturating_mul(u64::from(img.height));
        if pixels > self.left || img.width == 0 || img.height == 0 {
            self.over_budget += 1;
            return None;
        }
        self.left -= pixels;
        let scale = f64::from(THUMB_SIDE) / f64::from(img.width.max(img.height));
        let (w, h) = if scale < 1.0 {
            (((f64::from(img.width) * scale).round() as u32).max(1), ((f64::from(img.height) * scale).round() as u32).max(1))
        } else {
            (img.width, img.height)
        };
        let small = if (w, h) == (img.width, img.height) { None } else { Some(web_common::resize(img, w, h)) };
        let png = web_common::encode_png(small.as_ref().unwrap_or(img)).ok()?;
        let rel = Thumbs::rel(id);
        let path = self.root.join(&rel);
        std::fs::create_dir_all(path.parent()?).ok()?;
        write_atomic(&path, &png).ok()?;
        self.made += 1;
        Some(rel)
    }

    /// Make item `id`'s thumbnail from a picture file (PNG, JPEG, WebP or GIF), unless an
    /// up-to-date one is there.
    pub fn from_file(&mut self, id: &str, file: &Path) -> Option<String> {
        if let Some(rel) = self.existing(id, Some(file)) {
            return Some(rel);
        }
        let ext = file.extension()?.to_str()?.to_ascii_lowercase();
        if !matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "webp" | "gif") || std::fs::metadata(file).ok()?.len() > MAX_THUMB_FILE {
            return None;
        }
        let (w, h) = picture_size(file)?;
        if u64::from(w).saturating_mul(u64::from(h)) > self.left {
            self.over_budget += 1;
            return None;
        }
        let bytes = std::fs::read(file).ok()?;
        let img = web_common::decode_picture(&bytes).ok()?;
        self.from_image(id, &img)
    }
}

/// What a build scan found.
pub struct BuildScan {
    pub items: Vec<Item>,
    pub notes: Vec<String>,
}

/// Every asset file in the project's build (app/, or the project folder when the app is edited
/// in place), with the generated ones from assets.json marked `generated`. Bounded: at most
/// [`MAX_ITEMS`] items and [`MAX_ENTRIES`] files and folders looked at.
pub fn scan_build(ws: &Workspace, thumbs: &mut Thumbs, cancel: &CancelToken, report: &dyn Fn(usize, &str)) -> Result<BuildScan, ToolError> {
    let app = ws.app_dir();
    let mut notes = Vec::new();
    let mut items: Vec<Item> = Vec::new();
    if !app.is_dir() {
        notes.push(format!("There's no {} yet: nothing has been built.", ws.app_place()));
        return Ok(BuildScan { items, notes });
    }
    let in_root = ws.manifest.app_in_root;
    let (mut seen, mut stack, mut capped, mut deep) = (0usize, vec![(app.clone(), 0usize)], false, false);
    while let Some((dir, depth)) = stack.pop() {
        cancel.check()?;
        let Ok(entries) = std::fs::read_dir(&dir) else {
            notes.push(format!("{} couldn't be read.", relative(&ws.root, &dir).unwrap_or_default()));
            continue;
        };
        for entry in entries.flatten() {
            seen += 1;
            if seen > MAX_ENTRIES {
                capped = true;
                break;
            }
            let Ok(ft) = entry.file_type() else { continue };
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                // The project's own records sit beside the app when the app is the project folder.
                let own = in_root && dir == app && OWN_NAMES.contains(&name.as_str());
                if skipped_folder(&name) || own {
                    continue;
                }
                if depth >= MAX_DEPTH {
                    deep = true;
                    continue;
                }
                stack.push((path, depth + 1));
                continue;
            }
            if !ft.is_file() || (in_root && dir == app && OWN_NAMES.contains(&name.as_str())) {
                continue;
            }
            let Some(rel) = relative(&ws.root, &path) else { continue };
            let Some(kind) = classify(&rel) else { continue };
            if items.len() >= MAX_ITEMS {
                capped = true;
                break;
            }
            let bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
            let mut item = Item::new(Source::Build, &rel, kind, name, build_status(&rel));
            item.path = Some(rel.clone());
            item.bytes = bytes;
            describe_file(&mut item, &path, thumbs);
            if items.len().is_multiple_of(200) {
                report(items.len(), &rel);
            }
            items.push(item);
        }
        if capped {
            break;
        }
    }
    if capped {
        notes.push(format!("The scan stopped at {MAX_ITEMS} assets or {MAX_ENTRIES} files and folders; the rest of the build isn't listed."));
    }
    if deep {
        notes.push(format!("Folders more than {MAX_DEPTH} deep weren't looked in."));
    }
    merge_generated(ws, &mut items, thumbs);
    Ok(BuildScan { items, notes })
}

/// Mark the build's generated assets (assets.json) `generated`, adding any the scan didn't reach
/// (a skipped folder) that are still there.
pub fn merge_generated(ws: &Workspace, items: &mut Vec<Item>, thumbs: &mut Thumbs) {
    let app = ws.app_dir();
    for rec in ws.assets() {
        let file = app.join(&rec.file);
        let Some(rel) = relative(&ws.root, &file) else { continue };
        if !crate::workspace::inside(&app, &file) {
            continue;
        }
        let made_from = format!("Generated ({}) from: {}", rec.kind, rec.prompt.chars().take(300).collect::<String>());
        if let Some(item) = items.iter_mut().find(|i| i.path.as_deref() == Some(rel.as_str())) {
            item.status = Status::Generated;
            item.detail = Some(made_from);
            continue;
        }
        let Ok(meta) = std::fs::metadata(&file) else { continue };
        if !meta.is_file() || items.len() >= MAX_ITEMS {
            continue;
        }
        let kind = classify(&rel).unwrap_or(if rec.kind == "model" { Kind::Model } else { Kind::Texture });
        let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut item = Item::new(Source::Build, &rel, kind, name, Status::Generated);
        item.path = Some(rel);
        item.bytes = meta.len();
        item.detail = Some(made_from);
        describe_file(&mut item, &file, thumbs);
        items.push(item);
    }
}

/// Fill in what can be read cheaply from the file: a picture's size and thumbnail, a sound's
/// length. A file that can't be read gets a `problem`, never an error.
pub fn describe_file(item: &mut Item, file: &Path, thumbs: &mut Thumbs) {
    let ext = file.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).unwrap_or_default();
    match item.kind {
        Kind::Texture | Kind::Sprite if ext != "uasset" => match picture_size(file) {
            Some((w, h)) => {
                item.width = Some(w);
                item.height = Some(h);
                item.thumb = thumbs.from_file(&item.id, file);
            }
            None if matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tga" | "dds" | "psd") => {
                item.problem = Some(format!("This .{ext} file couldn't be read as a picture."));
            }
            None => {}
        },
        Kind::Sound | Kind::Music => match sound_length(file) {
            Some(secs) => {
                item.duration_secs = Some(secs);
                if item.kind == Kind::Sound && is_music(item.path.as_deref().unwrap_or(&item.name), Some(secs)) {
                    item.kind = Kind::Music;
                }
            }
            None if matches!(ext.as_str(), "wav" | "ogg" | "opus") => item.problem = Some(format!("This .{ext} file's header couldn't be read.")),
            None => {}
        },
        _ => {}
    }
}

// ---- Reading headers cheaply ----

/// Read at most `n` bytes from `at` (from the end when `at` is negative).
fn read_part(file: &Path, at: i64, n: usize) -> Option<Vec<u8>> {
    let mut f = std::fs::File::open(file).ok()?;
    let len = f.metadata().ok()?.len();
    let start = if at >= 0 { at.unsigned_abs().min(len) } else { len.saturating_sub(at.unsigned_abs()) };
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::with_capacity(n.min(1 << 20));
    f.take(n as u64).read_to_end(&mut buf).ok()?;
    Some(buf)
}

fn u16le(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}
fn u32le(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}
fn u32be(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}
fn u64le(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// A picture's width and height from its header, without decoding it.
pub fn picture_size(file: &Path) -> Option<(u32, u32)> {
    let head = read_part(file, 0, 64)?;
    let size = if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        (u32be(&head, 16)?, u32be(&head, 20)?)
    } else if head.starts_with(b"BM") {
        let h = u32le(&head, 22)? as i32;
        (u32le(&head, 18)?, h.unsigned_abs())
    } else if head.starts_with(b"DDS ") {
        (u32le(&head, 16)?, u32le(&head, 12)?)
    } else if head.starts_with(b"8BPS") {
        (u32be(&head, 18)?, u32be(&head, 14)?)
    } else if head.starts_with(b"GIF8") || head.starts_with(b"RIFF") || head.starts_with(&[0xff, 0xd8]) {
        let r = image::ImageReader::open(file).ok()?.with_guessed_format().ok()?;
        r.into_dimensions().ok()?
    } else if file.extension().is_some_and(|e| e.eq_ignore_ascii_case("tga")) {
        // TGA has no mark: its width and height are at 12 and 14, after a known image type.
        let kind = *head.get(2)?;
        if !matches!(kind, 1 | 2 | 3 | 9 | 10 | 11) {
            return None;
        }
        (u32::from(u16le(&head, 12)?), u32::from(u16le(&head, 14)?))
    } else {
        return None;
    };
    (size.0 > 0 && size.1 > 0 && size.0 <= 1 << 20 && size.1 <= 1 << 20).then_some(size)
}

/// A sound's length in seconds from its headers: WAV (its format and data chunks) and Ogg
/// (Vorbis or Opus: the sample rate, and the last page's position).
pub fn sound_length(file: &Path) -> Option<f64> {
    let head = read_part(file, 0, 64 << 10)?;
    let secs = if head.starts_with(b"RIFF") && head.get(8..12) == Some(b"WAVE") {
        wav_length(&head, std::fs::metadata(file).ok()?.len())?
    } else if head.starts_with(b"OggS") {
        let rate = if find(&head, b"\x01vorbis").is_some() {
            let at = find(&head, b"\x01vorbis")?;
            f64::from(u32le(&head, at + 12)?)
        } else if find(&head, b"OpusHead").is_some() {
            48_000.0
        } else {
            return None;
        };
        let pre_skip = find(&head, b"OpusHead").and_then(|at| u16le(&head, at + 10)).map_or(0, u64::from);
        let tail = read_part(file, -(64 << 10), 64 << 10)?;
        let last = rfind(&tail, b"OggS")?;
        let granule = u64le(&tail, last + 6)?;
        if rate <= 0.0 || granule == u64::MAX {
            return None;
        }
        granule.saturating_sub(pre_skip) as f64 / rate
    } else {
        return None;
    };
    (secs.is_finite() && (0.0..=86_400.0).contains(&secs)).then_some(secs)
}

/// A WAV's length: its data chunk's size over its byte rate.
fn wav_length(head: &[u8], file_len: u64) -> Option<f64> {
    let (mut at, mut byte_rate) = (12usize, None);
    while at + 8 <= head.len() {
        let id = head.get(at..at + 4)?;
        let size = u32le(head, at + 4)?;
        if id == b"fmt " {
            byte_rate = u32le(head, at + 16);
        } else if id == b"data" {
            // A writer that streamed the file may leave the size unset; the rest of the file is data.
            let data = if size == 0 || size == u32::MAX { file_len.saturating_sub(at as u64 + 8) } else { u64::from(size) };
            let rate = f64::from(byte_rate.filter(|r| *r > 0)?);
            return Some(data as f64 / rate);
        }
        at = at.checked_add(8)?.checked_add(size as usize + (size as usize & 1))?;
    }
    None
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}
fn rfind(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).rposition(|w| w == needle)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::workspace::{AssetRecord, TargetRef};

    pub(crate) fn png(w: u32, h: u32) -> Vec<u8> {
        web_common::encode_png(&Image { width: w, height: h, rgba: vec![200; (w * h * 4) as usize] }).unwrap()
    }

    /// A WAV of `secs` seconds of silence at 8 kHz, 8-bit mono.
    pub(crate) fn wav(secs: u32) -> Vec<u8> {
        let data = 8000 * secs;
        let mut v = b"RIFF".to_vec();
        v.extend((36 + data).to_le_bytes());
        v.extend(b"WAVEfmt ");
        v.extend(16u32.to_le_bytes());
        v.extend(1u16.to_le_bytes()); // PCM
        v.extend(1u16.to_le_bytes()); // mono
        v.extend(8000u32.to_le_bytes());
        v.extend(8000u32.to_le_bytes()); // byte rate
        v.extend(1u16.to_le_bytes());
        v.extend(8u16.to_le_bytes());
        v.extend(b"data");
        v.extend(data.to_le_bytes());
        v.extend(vec![128u8; data as usize]);
        v
    }

    /// A minimal binary glTF.
    pub(crate) fn glb() -> Vec<u8> {
        let json = br#"{"asset":{"version":"2.0"}}  "#;
        let mut v = b"glTF".to_vec();
        v.extend(2u32.to_le_bytes());
        v.extend((12 + 8 + json.len() as u32).to_le_bytes());
        v.extend((json.len() as u32).to_le_bytes());
        v.extend(b"JSON");
        v.extend(json);
        v
    }

    fn project() -> (tempfile::TempDir, Workspace) {
        let d = tempfile::tempdir().unwrap();
        let ws = Workspace::create_at(d.path(), "Inv", "make it", TargetRef::None, None).unwrap();
        (d, ws)
    }

    fn put(ws: &Workspace, rel: &str, bytes: &[u8]) {
        let p = ws.app_dir().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }

    fn scan(ws: &Workspace) -> BuildScan {
        scan_build(ws, &mut Thumbs::new(&ws.root), &CancelToken::default(), &|_, _| {}).unwrap()
    }

    #[test]
    fn files_are_sorted_by_extension_folder_and_name() {
        assert_eq!(classify("app/Content/Hero.png"), Some(Kind::Texture));
        assert_eq!(classify("app/ui/Sprites/coin.PNG"), Some(Kind::Sprite));
        assert_eq!(classify("app/models/crate.glb"), Some(Kind::Model));
        assert_eq!(classify("app/audio/hit.wav"), Some(Kind::Sound));
        assert_eq!(classify("app/audio/Music/level1.ogg"), Some(Kind::Music));
        assert_eq!(classify("app/fonts/Body.woff2"), Some(Kind::Font));
        assert_eq!(classify("app/intro.webm"), Some(Kind::Video));
        assert_eq!(classify("app/Assets/Wall.mat"), Some(Kind::Material));
        assert_eq!(classify("app/Assets/Level.unity"), Some(Kind::Map));
        assert_eq!(classify("app/scenes/main.tscn"), Some(Kind::EngineAsset));
        assert_eq!(classify("app/src/main.rs"), None);
        assert_eq!(classify("app/README"), None);
        // Unreal packages: name prefix first, then folder.
        assert_eq!(classify("app/Content/Textures/Rock.uasset"), Some(Kind::Texture));
        assert_eq!(classify("app/Content/Props/T_Rock_D.uasset"), Some(Kind::Texture));
        assert_eq!(classify("app/Content/Props/SM_Rock.uasset"), Some(Kind::Model));
        assert_eq!(classify("app/Content/Chars/SK_Dwarf.uasset"), Some(Kind::Model));
        assert_eq!(classify("app/Content/Meshes/Rock.uasset"), Some(Kind::Model));
        assert_eq!(classify("app/Content/Audio/Hit.uasset"), Some(Kind::Sound));
        assert_eq!(classify("app/Content/Sfx/S_Hit.uasset"), Some(Kind::Sound));
        assert_eq!(classify("app/Content/Maps/Arena.uasset"), Some(Kind::Map));
        assert_eq!(classify("app/Content/Arena.umap"), Some(Kind::Map));
        assert_eq!(classify("app/Content/Props/M_Rock.uasset"), Some(Kind::Material));
        assert_eq!(classify("app/Content/Props/MI_Rock_Wet.uasset"), Some(Kind::Material));
        assert_eq!(classify("app/Content/Blueprints/BP_Hero.uasset"), Some(Kind::EngineAsset));
    }

    #[test]
    fn build_output_folders_are_skipped() {
        let (_d, ws) = project();
        put(&ws, "Content/T_Rock.uasset", b"x");
        for skipped in SKIPPED_FOLDERS {
            put(&ws, &format!("{skipped}/inside.png"), &png(2, 2));
        }
        put(&ws, "Sub/intermediate/also.png", &png(2, 2));
        let found = scan(&ws);
        let paths: Vec<&str> = found.items.iter().filter_map(|i| i.path.as_deref()).collect();
        assert_eq!(paths, ["app/Content/T_Rock.uasset"], "every build-output folder is left out, any case");
    }

    #[test]
    fn pictures_and_sounds_are_measured_and_bad_files_noted() {
        let (_d, ws) = project();
        put(&ws, "img/big.png", &png(512, 128));
        put(&ws, "img/broken.png", b"\x89PNG not really");
        put(&ws, "sfx/beep.wav", &wav(2));
        put(&ws, "sfx/theme_loop.wav", &wav(1));
        put(&ws, "sfx/long.wav", &wav(61));
        put(&ws, "sfx/bad.ogg", b"OggS nothing");
        put(&ws, "m/crate.glb", &glb());
        let found = scan(&ws);
        let get = |n: &str| found.items.iter().find(|i| i.name == n).unwrap();
        let big = get("big.png");
        assert_eq!((big.width, big.height, big.kind, big.status), (Some(512), Some(128), Kind::Texture, Status::Placeholder));
        let thumb = big.thumb.as_deref().unwrap();
        assert_eq!(picture_size(&ws.root.join(thumb)), Some((256, 64)), "the thumbnail keeps the shape within 256 pixels");
        assert!(get("broken.png").problem.is_some() && get("broken.png").thumb.is_none());
        assert_eq!((get("beep.wav").duration_secs, get("beep.wav").kind), (Some(2.0), Kind::Sound));
        assert_eq!(get("theme_loop.wav").kind, Kind::Music, "its name says music");
        assert_eq!(get("long.wav").kind, Kind::Music, "a minute or more is music");
        assert!(get("bad.ogg").problem.is_some());
        assert_eq!(get("crate.glb").kind, Kind::Model);
    }

    #[test]
    fn generated_assets_are_marked_and_marks_survive_rescans() {
        let (_d, ws) = project();
        put(&ws, "art/sky.png", &png(4, 4));
        put(&ws, "art/rock.png", &png(4, 4));
        put(&ws, "Saved/gen.glb", &glb());
        ws.record_asset(AssetRecord { file: "art/sky.png".into(), kind: "image".into(), prompt: "a blue sky".into(), cost_usd: 0.1, at_ms: 1 }).unwrap();
        ws.record_asset(AssetRecord { file: "Saved/gen.glb".into(), kind: "model".into(), prompt: "a crate".into(), cost_usd: 0.1, at_ms: 2 }).unwrap();
        ws.record_asset(AssetRecord { file: "gone.png".into(), kind: "image".into(), prompt: "gone".into(), cost_usd: 0.1, at_ms: 3 }).unwrap();
        let mut inv = Inventory::default();
        inv.replace(Source::Build, scan(&ws).items);
        let sky = inv.items.iter().find(|i| i.name == "sky.png").unwrap();
        assert_eq!(sky.status, Status::Generated);
        assert!(sky.detail.as_deref().unwrap().contains("a blue sky"));
        let model = inv.items.iter().find(|i| i.name == "gen.glb").unwrap();
        assert_eq!((model.status, model.kind), (Status::Generated, Kind::Model), "a generated file in a skipped folder is still listed");
        assert!(!inv.items.iter().any(|i| i.name == "gone.png"), "a generated file that's gone isn't");

        // The person marks the rock; a rescan keeps it, and drops what's gone.
        let rock_id = inv.items.iter().find(|i| i.name == "rock.png").unwrap().id.clone();
        let rock = inv.items.iter_mut().find(|i| i.id == rock_id).unwrap();
        (rock.status, rock.note, rock.marked_ms) = (Status::NeedsReplacing, "too shiny".into(), Some(5));
        std::fs::remove_file(ws.app_dir().join("art/sky.png")).unwrap();
        put(&ws, "art/rock.png", &png(8, 8));
        inv.replace(Source::Build, scan(&ws).items);
        let rock = inv.items.iter().find(|i| i.id == rock_id).unwrap();
        assert_eq!((rock.status, rock.note.as_str(), rock.width), (Status::NeedsReplacing, "too shiny", Some(8)));
        assert!(!inv.items.iter().any(|i| i.name == "sky.png"));
        // An original item isn't touched by a build rescan.
        let mut orig = Item::new(Source::Original, "game#1", Kind::Texture, "Wall", Status::Original);
        orig.origin = Some("game#1".into());
        inv.items.push(orig);
        inv.replace(Source::Build, scan(&ws).items);
        assert_eq!(inv.of(Source::Original).count(), 1);
    }

    #[test]
    fn marks_made_during_a_scan_are_kept_when_it_saves() {
        let mut scanned = Inventory::default();
        scanned.items.push(Item::new(Source::Build, "app/a.png", Kind::Texture, "a.png", Status::Placeholder));
        let mut disk = scanned.clone();
        disk.items[0].status = Status::Replaced;
        disk.items[0].marked_ms = Some(9);
        scanned.keep_newer_marks(&disk);
        assert_eq!(scanned.items[0].status, Status::Replaced);
    }

    #[test]
    fn the_file_is_versioned_and_newer_files_are_never_written() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(load(d.path()).unwrap().version, INVENTORY_VERSION, "none yet is an empty current inventory");
        // A version-0 file (no version) is brought forward.
        let item = serde_json::to_value(Item::new(Source::Build, "app/a.png", Kind::Texture, "a.png", Status::Placeholder)).unwrap();
        std::fs::write(d.path().join(INVENTORY), serde_json::json!({"items": [item]}).to_string()).unwrap();
        let inv = load(d.path()).unwrap();
        assert_eq!((inv.version, inv.items.len()), (INVENTORY_VERSION, 1));
        save(d.path(), &inv).unwrap();
        assert_eq!(load(d.path()).unwrap(), inv);
        // A newer one is read as far as it's known, and never written over.
        std::fs::write(d.path().join(INVENTORY), serde_json::json!({"version": 99, "items": [], "future": true}).to_string()).unwrap();
        let newer = load(d.path()).unwrap();
        assert!(newer.is_newer());
        assert_eq!(save(d.path(), &inv).unwrap_err().code, ErrorCode::NotSupported);
        assert!(std::fs::read_to_string(d.path().join(INVENTORY)).unwrap().contains("future"));
        // Not JSON, or a bad version: an error, never a panic.
        std::fs::write(d.path().join(INVENTORY), b"{").unwrap();
        assert!(load(d.path()).is_err());
        assert!(parse(br#"{"version": "x"}"#).is_err());
        assert!(parse(br#"[1]"#).is_err());
    }

    #[test]
    fn the_published_schema_is_current() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/schemas/inventory.schema.json");
        let published = std::fs::read_to_string(path).unwrap_or_default().replace("\r\n", "\n");
        assert!(published == inventory_schema(), "docs/schemas/inventory.schema.json is stale: run `cargo xtask schemas`");
        assert!(published.contains("needs_replacing") && published.contains("engine_asset"));
    }

    #[test]
    fn ids_are_stable_and_differ_by_source() {
        assert_eq!(item_id(Source::Build, "app/a.png"), item_id(Source::Build, "app/a.png"));
        assert_ne!(item_id(Source::Build, "x"), item_id(Source::Original, "x"));
        assert_eq!(item_id(Source::Build, "x").len(), 16);
    }

    #[test]
    fn headers_that_lie_are_refused_quietly() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("x.wav");
        let mut bad = wav(1);
        bad[40..44].copy_from_slice(&u32::MAX.to_le_bytes());
        bad[28..32].copy_from_slice(&0u32.to_le_bytes()); // byte rate 0
        std::fs::write(&f, &bad).unwrap();
        assert_eq!(sound_length(&f), None);
        let p = d.path().join("x.png");
        let mut big = png(1, 1);
        big[16..20].copy_from_slice(&u32::MAX.to_be_bytes());
        std::fs::write(&p, &big).unwrap();
        assert_eq!(picture_size(&p), None);
        assert_eq!(picture_size(&d.path().join("missing.png")), None);
    }
}
