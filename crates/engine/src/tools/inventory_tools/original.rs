//! Taking the original's assets out into `reference/assets/<kind>/` for the inventory (Deep mode
//! only: the caller checks). Each kind of game is an [`OriginalReader`] in [`READERS`]; a fork adds
//! one there. Bounded: [`BYTE_BUDGET`] bytes written and [`MAX_ITEMS`] items, cancellable, and
//! resumable: a file already taken out is used as it is, never made again.

use crate::inventory::{self, Item, Kind, MAX_ITEMS, Source, Status, Thumbs, classify, is_music, relative};
use crate::project::write_atomic;
use crate::tool::Ctx;
use crate::tools::game_archive_tools::{
    PackageSource, Serialized, archive_kind, known_version, open_pak, readable, shown, take_out, unity_texture_image, unreal_texture_image,
};
use crate::tools::web_common::encode_png;
use crate::workspace::{TargetRef, Workspace};
use craft_anything_evidence::{ErrorCode, ToolError};
use craft_anything_formats::unity_serialized;
use std::path::{Path, PathBuf};

/// Bytes the original's assets may take in reference/assets/, every run together.
pub const BYTE_BUDGET: u64 = 4 << 30;
/// How deep the original's folder is looked through, and how many entries at most.
const MAX_DEPTH: usize = 8;
const MAX_ENTRIES: usize = 50_000;
/// The largest single sound taken out of a Unity resource file.
const MAX_SOUND: u64 = 256 << 20;
/// Engine package extensions the loose-file reader leaves alone.
const ENGINE_PACKAGES: &[&str] = &["uasset", "umap", "prefab", "mat", "asset", "unity", "tres", "tscn"];
/// The largest loose file copied.
const MAX_LOOSE: u64 = 1 << 30;
/// How much of an object is read for its name.
const NAME_READ: u64 = 1024;

/// One kind of original the inventory can take assets out of.
pub trait OriginalReader: Sync {
    /// Its name in notes ("Unity", "Unreal").
    fn name(&self) -> &'static str;
    /// Take what it recognises in `job.files` out into the project, adding an item for each.
    fn read(&self, job: &mut Job<'_, '_>) -> Result<(), ToolError>;
}

/// The readers, in order. Loose files go last so the engines' archives have the item budget first.
pub const READERS: &[&dyn OriginalReader] = &[&Unity, &UnrealPak, &Others, &Loose];

/// A scan of the original in progress.
pub struct Job<'a, 'c> {
    ctx: &'a Ctx<'c>,
    /// The original's folder.
    pub game: PathBuf,
    /// The project folder.
    root: PathBuf,
    /// Every file in the original's folder (bounded walk), sorted.
    pub files: Vec<PathBuf>,
    thumbs: &'a mut Thumbs,
    /// The person may unpack this game (needed for Unreal textures, as for game_unreal_texture_export).
    permission: bool,
    pub items: Vec<Item>,
    /// Items listed without a file (models and the like), added after the rest while there's room.
    listed: Vec<Item>,
    pub notes: Vec<String>,
    /// Bytes of the files this run uses (taken out now or before).
    pub bytes: u64,
    pub taken_out: u64,
    pub already_there: u64,
    over_budget: u64,
    full: bool,
}

/// What a scan of the original found.
pub struct OriginalScan {
    pub folder: PathBuf,
    pub items: Vec<Item>,
    pub notes: Vec<String>,
    pub bytes: u64,
    pub taken_out: u64,
    pub already_there: u64,
}

/// The original's folder: the target's folder, or the folder of a target file.
pub fn original_folder(ws: &Workspace) -> Result<PathBuf, String> {
    match &ws.manifest.target {
        TargetRef::Local { path } if path.is_dir() => Ok(path.clone()),
        TargetRef::Local { path } if path.is_file() => path.parent().map(Path::to_path_buf).ok_or_else(|| "The original has no folder.".into()),
        TargetRef::Local { path } => Err(format!("The original ({}) isn't on this computer any more.", path.display())),
        TargetRef::Website { .. } => Err("The original is a website; its pictures come from web_site_capture, not from here.".into()),
        TargetRef::None => Err("This project has no original to take assets from.".into()),
    }
}

/// Take the original's assets out into the project and list them.
pub fn scan(ctx: &Ctx<'_>, ws: &Workspace, thumbs: &mut Thumbs, permission: bool) -> Result<OriginalScan, ToolError> {
    let game = original_folder(ws).map_err(|why| ToolError::new(ErrorCode::NotFound, why))?;
    ctx.report(None, format!("Looking through {}", game.display()));
    let (files, capped) = walk(&game, ctx)?;
    let mut job = Job {
        ctx,
        game: game.clone(),
        root: ws.root.clone(),
        files,
        thumbs,
        permission,
        items: Vec::new(),
        listed: Vec::new(),
        notes: Vec::new(),
        bytes: 0,
        taken_out: 0,
        already_there: 0,
        over_budget: 0,
        full: false,
    };
    if capped {
        job.notes.push(format!("Only the first {MAX_ENTRIES} files and folders of the original were looked at (and {MAX_DEPTH} folders deep)."));
    }
    for reader in READERS {
        ctx.cancel.check()?;
        if let Err(e) = reader.read(&mut job) {
            if e.code == ErrorCode::Cancelled {
                return Err(e);
            }
            job.notes.push(format!("{}: {}", reader.name(), e.message));
        }
    }
    let room = MAX_ITEMS.saturating_sub(job.items.len());
    if job.listed.len() > room {
        job.full = true;
    }
    let listed = std::mem::take(&mut job.listed);
    job.items.extend(listed.into_iter().take(room));
    if job.full {
        job.notes.push(format!("The inventory holds at most {MAX_ITEMS} of the original's assets; the rest aren't listed."));
    }
    if job.over_budget > 0 {
        job.notes.push(format!(
            "{} asset(s) weren't taken out: the original's assets may take {} GiB in reference/assets/, and that's used up.",
            job.over_budget,
            BYTE_BUDGET >> 30
        ));
    }
    Ok(OriginalScan { folder: game, items: job.items, notes: job.notes, bytes: job.bytes, taken_out: job.taken_out, already_there: job.already_there })
}

/// Every file under `game`, sorted, bounded; true when the walk stopped early.
fn walk(game: &Path, ctx: &Ctx<'_>) -> Result<(Vec<PathBuf>, bool), ToolError> {
    let (mut files, mut seen, mut capped, mut stack) = (Vec::new(), 0usize, false, vec![(game.to_path_buf(), 0usize)]);
    while let Some((dir, depth)) = stack.pop() {
        ctx.cancel.check()?;
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            seen += 1;
            if seen > MAX_ENTRIES {
                capped = true;
                break;
            }
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                if depth < MAX_DEPTH {
                    stack.push((entry.path(), depth + 1));
                } else {
                    capped = true;
                }
            } else if ft.is_file() {
                files.push(entry.path());
            }
        }
        if seen > MAX_ENTRIES {
            break;
        }
    }
    files.sort();
    Ok((files, capped))
}

impl Job<'_, '_> {
    fn has_room(&mut self) -> bool {
        if self.items.len() >= MAX_ITEMS {
            self.full = true;
        }
        !self.full
    }

    /// Whether `more` bytes still fit the budget.
    fn fits(&self, more: u64) -> bool {
        self.bytes.saturating_add(more) <= BYTE_BUDGET
    }

    fn rel(&self, p: &Path) -> String {
        relative(&self.game, p).unwrap_or_else(|| p.display().to_string())
    }

    /// Use a file already taken out: its size and what can be read from it.
    fn reuse(&mut self, item: &mut Item, file: &Path) {
        item.bytes = std::fs::metadata(file).map(|m| m.len()).unwrap_or(0);
        self.bytes = self.bytes.saturating_add(item.bytes);
        self.already_there += 1;
        inventory::describe_file(item, file, self.thumbs);
    }

    /// Write a new file taken out; a failure is the item's problem.
    fn write(&mut self, item: &mut Item, rel: &str, bytes: &[u8]) -> bool {
        let path = self.root.join(rel);
        let made =
            path.parent().map_or(Ok(()), std::fs::create_dir_all).map_err(|e| ToolError::io(path.display(), e)).and_then(|()| write_atomic(&path, bytes));
        match made {
            Ok(()) => {
                item.path = Some(rel.to_string());
                item.bytes = bytes.len() as u64;
                self.bytes = self.bytes.saturating_add(item.bytes);
                self.taken_out += 1;
                true
            }
            Err(e) => {
                item.problem = Some(format!("It couldn't be saved: {}", e.message));
                false
            }
        }
    }

    fn over_budget(&mut self, item: &mut Item) {
        self.over_budget += 1;
        item.problem = Some(format!("Not taken out: the {} GiB budget for the original's assets is used up.", BYTE_BUDGET >> 30));
    }
}

/// A file or folder name made safe on every system: letters, digits, `-`, `_` and `.`, at most
/// `max` characters, never a reserved Windows name.
pub fn safe_name(s: &str, max: usize) -> String {
    let mut out: String = s.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' }).take(max).collect();
    out = out.trim_matches(['.', ' ']).to_string();
    if out.is_empty() {
        out = "unnamed".into();
    }
    let stem = out.split('.').next().unwrap_or_default().to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT")) && stem.len() == 4 && stem.ends_with(|c: char| c.is_ascii_digit()));
    if reserved { format!("_{out}") } else { out }
}

/// Where an asset of `kind` from `archive` goes, relative to the project folder.
fn out_rel(kind: Kind, archive: &str, file: &str) -> String {
    format!("reference/assets/{}/{}/{}", kind.id(), safe_name(archive, 80), file)
}

// ---- Unity ----

/// Unity games: textures (as PNG) and sounds (as the game stores them) from the serialized files
/// and bundles; meshes, fonts and videos are listed.
pub struct Unity;

/// A Unity class the inventory lists, and what kind of asset it is.
const UNITY_CLASSES: &[(i32, Kind)] = &[(28, Kind::Texture), (83, Kind::Sound), (43, Kind::Model), (128, Kind::Font), (329, Kind::Video)];

impl OriginalReader for Unity {
    fn name(&self) -> &'static str {
        "Unity"
    }

    fn read(&self, job: &mut Job<'_, '_>) -> Result<(), ToolError> {
        let archives: Vec<PathBuf> = job.files.iter().filter(|p| archive_kind(p).is_some_and(|(k, _)| k.starts_with("unity"))).cloned().collect();
        let total = archives.len().max(1);
        for (n, path) in archives.iter().enumerate() {
            job.ctx.cancel.check()?;
            if !job.has_room() {
                break;
            }
            let rel = job.rel(path);
            job.ctx.report(Some(n as f32 / total as f32), format!("Taking assets out of {rel}"));
            let raw = path.display().to_string();
            let first = match Serialized::open(&raw, None) {
                Ok(s) => s,
                Err(e) => {
                    job.notes.push(format!("{rel} couldn't be read: {}", e.message));
                    continue;
                }
            };
            let inner: Vec<Option<String>> = match &first {
                Serialized::Bundle { nodes, .. } => nodes.iter().filter(|n| n.flags & 4 != 0).map(|n| Some(n.path.clone())).collect(),
                Serialized::File { .. } => vec![None],
            };
            let mut first = Some(first);
            for file in &inner {
                // The first serialized file is the one open already.
                let opened = match first.take() {
                    Some(s) => Ok(s),
                    None => Serialized::open(&raw, file.as_deref()),
                };
                let result = opened.and_then(|mut s| unity_file(job, path, &rel, file.as_deref(), &mut s));
                match result {
                    Err(e) if e.code == ErrorCode::Cancelled => return Err(e),
                    Err(e) => {
                        job.notes.push(format!("{rel}{} couldn't be read: {}", file.as_deref().map(|f| format!(" ({f})")).unwrap_or_default(), e.message))
                    }
                    Ok(()) => {}
                }
            }
        }
        Ok(())
    }
}

/// The assets of one Unity serialized file.
fn unity_file(job: &mut Job<'_, '_>, path: &Path, rel: &str, inner: Option<&str>, src: &mut Serialized) -> Result<(), ToolError> {
    let head = src.read(0, src.len().min(unity_serialized::HEADER_READ as u64))?;
    let h = unity_serialized::header(&head, src.len()).map_err(|_| ToolError::unsupported("it isn't a Unity serialized file this build reads"))?;
    let meta = src.read(0, h.metadata_offset.saturating_add(h.metadata_size))?;
    let m = unity_serialized::metadata(&meta, &h).map_err(|e| ToolError::unsupported(format!("its object table can't be read ({e})")))?;
    let version = known_version(&m.unity_version, path);
    let archive = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let objects: Vec<_> = m.objects.iter().filter_map(|o| UNITY_CLASSES.iter().find(|(c, _)| *c == o.class_id).map(|(_, k)| (o, *k))).collect();
    for (n, (o, kind)) in objects.iter().enumerate() {
        job.ctx.cancel.check()?;
        if !job.has_room() {
            return Ok(());
        }
        if n % 50 == 0 && n > 0 {
            job.ctx.report(None, format!("{rel}: {n} of {} assets", objects.len()));
        }
        let head = src.read(o.offset, u64::from(o.size).min(NAME_READ)).unwrap_or_default();
        let class = unity_serialized::class_name(o.class_id);
        let name = unity_serialized::object_name(o.class_id, &h, &head).unwrap_or_else(|| format!("{class} {}", o.path_id));
        let origin = format!("{rel}{}#{}", inner.map(|f| format!(":{f}")).unwrap_or_default(), o.path_id);
        let mut item = Item::new(Source::Original, &origin, *kind, name.clone(), Status::Original);
        item.origin = Some(origin);
        item.bytes = u64::from(o.size);
        let file = format!("{}_{}", safe_name(&name, 80), o.path_id);
        match kind {
            Kind::Texture => unity_texture(job, &mut item, src, &h, &version, o, &archive, &file),
            Kind::Sound => unity_sound(job, &mut item, src, &h, &version, o, &archive, &file),
            _ => {
                item.detail = Some(format!("Unity {class}: listed; taking it out isn't supported yet."));
                job.listed.push(item);
                continue;
            }
        }
        job.items.push(item);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn unity_texture(
    job: &mut Job<'_, '_>,
    item: &mut Item,
    src: &mut Serialized,
    h: &unity_serialized::Header,
    version: &str,
    o: &unity_serialized::Object,
    archive: &str,
    file: &str,
) {
    let rel = out_rel(Kind::Texture, archive, &format!("{file}.png"));
    let path = job.root.join(&rel);
    if path.is_file() {
        item.path = Some(rel);
        job.reuse(item, &path);
        return;
    }
    if !job.fits(0) {
        return job.over_budget(item);
    }
    match unity_texture_image(src, version, h.big_endian, o) {
        Ok((t, img)) => {
            item.width = Some(t.width);
            item.height = Some(t.height);
            item.detail = Some(format!("Unity Texture2D ({})", craft_anything_formats::unity_texture::format_name(t.format)));
            let png = match encode_png(&img) {
                Ok(png) => png,
                Err(e) => {
                    item.problem = Some(e.message);
                    return;
                }
            };
            if !job.fits(png.len() as u64) {
                return job.over_budget(item);
            }
            if job.write(item, &rel, &png) {
                item.thumb = job.thumbs.from_image(&item.id, &img);
            }
        }
        Err(e) => item.problem = Some(e.message),
    }
}

/// A Unity AudioClip's fields (Unity 5 and later, no type tree).
#[derive(Debug, Clone, PartialEq)]
pub struct Clip {
    pub name: String,
    pub channels: i32,
    pub frequency: i32,
    pub length_secs: f32,
    /// Where its data is: a file beside it (`.resource`) or in the same bundle.
    pub source: String,
    pub offset: u64,
    pub size: u64,
    pub compression: i32,
}

/// A bounds-checked cursor over an object's bytes.
struct Cur<'b> {
    data: &'b [u8],
    pos: usize,
    big: bool,
}

impl<'b> Cur<'b> {
    fn take(&mut self, n: usize) -> Option<&'b [u8]> {
        let s = self.data.get(self.pos..self.pos.checked_add(n)?)?;
        self.pos += n;
        Some(s)
    }
    fn u32(&mut self) -> Option<u32> {
        let b: [u8; 4] = self.take(4)?.try_into().ok()?;
        Some(if self.big { u32::from_be_bytes(b) } else { u32::from_le_bytes(b) })
    }
    fn u64(&mut self) -> Option<u64> {
        let b: [u8; 8] = self.take(8)?.try_into().ok()?;
        Some(if self.big { u64::from_be_bytes(b) } else { u64::from_le_bytes(b) })
    }
    fn align4(&mut self) -> Option<()> {
        let pad = (4 - self.pos % 4) % 4;
        self.take(pad).map(|_| ())
    }
    fn string(&mut self) -> Option<String> {
        let n = self.u32()? as usize;
        if n > 4096 {
            return None;
        }
        let s = String::from_utf8_lossy(self.take(n)?).into_owned();
        self.align4()?;
        Some(s)
    }
}

/// Read an AudioClip object written by Unity `version` (major, minor). None when the bytes don't
/// make a sensible clip (a layout this doesn't know is refused, not read wrong).
pub fn parse_clip(object: &[u8], big_endian: bool, version: (u32, u32)) -> Option<Clip> {
    let mut c = Cur { data: object, pos: 0, big: big_endian };
    let name = c.string()?;
    let _load_type = c.u32()?;
    let channels = c.u32()? as i32;
    let frequency = c.u32()? as i32;
    let _bits = c.u32()?;
    let length_secs = f32::from_bits(c.u32()?);
    c.take(1)?; // tracker format
    if version >= (2017, 1) {
        c.take(1)?; // ambisonic
    }
    c.align4()?;
    c.u32()?; // subsound index
    c.take(3)?; // preload, load in background, legacy 3D
    c.align4()?;
    let source = c.string()?;
    let offset = c.u64()?;
    let size = c.u64()?;
    let compression = c.u32()? as i32;
    let sane = (1..=32).contains(&channels) && (1000..=384_000).contains(&frequency) && length_secs.is_finite() && (0.0..=86_400.0).contains(&length_secs);
    sane.then_some(Clip { name, channels, frequency, length_secs, source, offset, size, compression })
}

/// What a sound's bytes are, from their first bytes: a file extension and words.
pub fn sound_format(head: &[u8]) -> (&'static str, String) {
    if head.starts_with(b"FSB5") {
        const CODECS: &[(u32, &str)] = &[
            (1, "PCM8"),
            (2, "PCM16"),
            (3, "PCM24"),
            (4, "PCM32"),
            (5, "PCM float"),
            (6, "GC ADPCM"),
            (7, "IMA ADPCM"),
            (11, "MPEG"),
            (15, "Vorbis"),
            (16, "FADPCM"),
            (17, "Opus"),
        ];
        let mode = head.get(0x18..0x1c).and_then(|b| b.try_into().ok()).map(u32::from_le_bytes).unwrap_or(0);
        let codec = CODECS.iter().find(|(m, _)| *m == mode).map_or_else(|| format!("codec {mode}"), |(_, n)| (*n).to_string());
        ("fsb", format!("an FMOD sound bank (FSB5, {codec}); an FSB-capable player or converter opens it"))
    } else if head.starts_with(b"OggS") {
        ("ogg", "Ogg".into())
    } else if head.starts_with(b"RIFF") {
        ("wav", "WAV".into())
    } else if head.starts_with(b"ID3") || head.get(..2).is_some_and(|b| b[0] == 0xff && b[1] & 0xe0 == 0xe0) {
        ("mp3", "MP3".into())
    } else {
        ("bin", "raw audio data".into())
    }
}

#[allow(clippy::too_many_arguments)]
fn unity_sound(
    job: &mut Job<'_, '_>,
    item: &mut Item,
    src: &mut Serialized,
    h: &unity_serialized::Header,
    version: &str,
    o: &unity_serialized::Object,
    archive: &str,
    file: &str,
) {
    let numbers = unity_serialized::version_numbers(version).unwrap_or((0, 0));
    let clip = src.read(o.offset, u64::from(o.size)).ok().and_then(|b| parse_clip(&b, h.big_endian, numbers));
    let Some(clip) = clip else {
        item.problem = Some(format!("This AudioClip's layout (Unity {version}) isn't one that's read here."));
        return;
    };
    item.duration_secs = Some(f64::from(clip.length_secs));
    if is_music(&clip.name, item.duration_secs) {
        item.kind = Kind::Music;
    }
    if clip.size == 0 || clip.source.is_empty() {
        item.problem = Some("Its sound data isn't kept in a resource file, so it isn't taken out.".into());
        return;
    }
    item.bytes = clip.size;
    if clip.size > MAX_SOUND {
        item.problem = Some(format!("It's {} bytes; sounds up to {MAX_SOUND} bytes are taken out.", clip.size));
        return;
    }
    let head = match src.stream(&clip.source, clip.offset, clip.size.min(64)) {
        Ok(b) => b,
        Err(e) => {
            item.problem = Some(e.message);
            return;
        }
    };
    let (ext, what) = sound_format(&head);
    item.detail = Some(format!("Unity AudioClip: {} channel(s), {} Hz, {what}", clip.channels, clip.frequency));
    let rel = out_rel(item.kind, archive, &format!("{file}.{ext}"));
    let path = job.root.join(&rel);
    if path.is_file() {
        item.path = Some(rel);
        item.bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(clip.size);
        job.bytes = job.bytes.saturating_add(item.bytes);
        job.already_there += 1;
        return;
    }
    if !job.fits(clip.size) {
        return job.over_budget(item);
    }
    match src.stream(&clip.source, clip.offset, clip.size) {
        Ok(bytes) => {
            job.write(item, &rel, &bytes);
        }
        Err(e) => item.problem = Some(e.message),
    }
}

// ---- Unreal ----

/// Unreal paks: textures as PNG (with `permission`), plain sound and picture files as they are;
/// other packages are listed.
pub struct UnrealPak;

impl OriginalReader for UnrealPak {
    fn name(&self) -> &'static str {
        "Unreal"
    }

    fn read(&self, job: &mut Job<'_, '_>) -> Result<(), ToolError> {
        let paks: Vec<PathBuf> = job.files.iter().filter(|p| archive_kind(p).is_some_and(|(k, _)| k == "unreal_pak")).cloned().collect();
        let mut needs_permission = 0usize;
        for path in &paks {
            job.ctx.cancel.check()?;
            if !job.has_room() {
                break;
            }
            let rel = job.rel(path);
            let mut pak = match open_pak(job.ctx, &path.display().to_string()) {
                Ok(p) if p.footer.encrypted_index => {
                    job.notes.push(format!("{rel}'s index is encrypted with the game's AES key, so its files can't be listed."));
                    continue;
                }
                Ok(p) => p,
                Err(e) => {
                    job.notes.push(format!("{rel} couldn't be read: {}", e.message));
                    continue;
                }
            };
            let archive = path.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let entries: Vec<_> = pak.files.iter().filter(|e| classify(shown(e)).is_some()).cloned().collect();
            for (n, e) in entries.iter().enumerate() {
                job.ctx.cancel.check()?;
                if !job.has_room() {
                    break;
                }
                if n % 100 == 0 {
                    job.ctx.report(None, format!("{rel}: {n} of {} assets", entries.len()));
                }
                let inner = shown(e).to_string();
                let Some(kind) = classify(&inner) else { continue };
                let origin = format!("{rel}:{inner}");
                let name = inner.rsplit('/').next().unwrap_or(&inner).to_string();
                let mut item = Item::new(Source::Original, &origin, kind, name.clone(), Status::Original);
                item.origin = Some(origin);
                item.bytes = e.uncompressed_size;
                let is_package = inner.to_ascii_lowercase().ends_with(".uasset") || inner.to_ascii_lowercase().ends_with(".umap");
                if !job.permission {
                    needs_permission += 1;
                    job.listed.push(item);
                    continue;
                }
                if is_package && kind == Kind::Texture {
                    let lower = inner.to_lowercase();
                    let stem = lower.strip_suffix(".uasset").unwrap_or(&lower).to_string();
                    let file = format!("{}.png", safe_name(name.strip_suffix(".uasset").unwrap_or(&name), 120));
                    let out = out_rel(kind, &archive, &safe_path(&inner, &file));
                    let target = job.root.join(&out);
                    if target.is_file() {
                        item.path = Some(out);
                        job.reuse(&mut item, &target);
                    } else if !job.fits(0) {
                        job.over_budget(&mut item);
                    } else {
                        let mut source = PackageSource::Pak { pak: Box::new(pak), stem };
                        let decoded = unreal_texture_image(&mut source, &inner, &job.ctx.cancel);
                        let PackageSource::Pak { pak: back, .. } = source else { return Err(ToolError::internal("the pak went missing")) };
                        pak = *back;
                        match decoded {
                            Ok(t) => {
                                item.width = Some(t.width);
                                item.height = Some(t.height);
                                item.detail = Some(format!("Unreal Texture2D ({})", t.pixel_format));
                                match encode_png(&t.image) {
                                    Ok(png) if job.fits(png.len() as u64) => {
                                        if job.write(&mut item, &out, &png) {
                                            item.thumb = job.thumbs.from_image(&item.id, &t.image);
                                        }
                                    }
                                    Ok(_) => job.over_budget(&mut item),
                                    Err(err) => item.problem = Some(err.message),
                                }
                            }
                            Err(err) if err.code == ErrorCode::Cancelled => return Err(err),
                            Err(err) => item.problem = Some(err.message),
                        }
                    }
                    job.items.push(item);
                } else if is_package {
                    item.detail = Some("An Unreal package: listed; only textures are taken out.".into());
                    job.listed.push(item);
                } else {
                    // A plain file (a sound, a picture, a font): taken out as it is.
                    let out = out_rel(kind, &archive, &safe_path(&inner, &safe_name(&name, 120)));
                    let target = job.root.join(&out);
                    if target.is_file() {
                        item.path = Some(out);
                        job.reuse(&mut item, &target);
                    } else if !job.fits(e.uncompressed_size) {
                        job.over_budget(&mut item);
                    } else if let Err(err) = readable(e) {
                        item.problem = Some(err.message);
                    } else {
                        let made = target.parent().map_or(Ok(()), std::fs::create_dir_all).map_err(|err| ToolError::io(target.display(), err));
                        match made.and_then(|()| take_out(&mut pak, e, &target, &job.ctx.cancel)) {
                            Ok((size, _)) => {
                                item.path = Some(out);
                                job.bytes = job.bytes.saturating_add(size);
                                job.taken_out += 1;
                                inventory::describe_file(&mut item, &target, job.thumbs);
                                item.bytes = size;
                            }
                            Err(err) if err.code == ErrorCode::Cancelled => return Err(err),
                            Err(err) => item.problem = Some(err.message),
                        }
                    }
                    job.items.push(item);
                }
            }
            if !pak.lims.is_empty() {
                job.notes.extend(pak.lims.iter().map(|l| format!("{rel}: {}", l.message)));
            }
        }
        if needs_permission > 0 {
            job.notes.push(format!(
                "{needs_permission} Unreal asset(s) are listed but not taken out: that needs permission: true (you made the game, or its rights holder allows unpacking it)."
            ));
        }
        Ok(())
    }
}

/// The folders of a path inside an archive, made safe, followed by `file`.
fn safe_path(inner: &str, file: &str) -> String {
    let mut parts: Vec<String> = inner.split('/').filter(|p| !p.is_empty() && *p != "." && *p != "..").map(|p| safe_name(p, 80)).collect();
    parts.pop();
    // Keep the last few folders: enough to tell files apart without very long paths.
    let keep = parts.len().saturating_sub(4);
    let mut out: Vec<String> = parts.split_off(keep);
    out.push(file.to_string());
    out.join("/")
}

// ---- What isn't read yet ----

/// Archives the inventory doesn't take apart yet (IoStore, Godot packs): said in the notes.
pub struct Others;

impl OriginalReader for Others {
    fn name(&self) -> &'static str {
        "Other archives"
    }

    fn read(&self, job: &mut Job<'_, '_>) -> Result<(), ToolError> {
        let mut by_kind: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
        for p in &job.files {
            if let Some((kind, _)) = archive_kind(p).filter(|(k, _)| matches!(*k, "unreal_iostore" | "godot_pck")) {
                *by_kind.entry(kind).or_default() += 1;
            }
        }
        for (kind, n) in by_kind {
            let what = if kind == "unreal_iostore" { "Unreal IoStore container(s) (.utoc/.ucas)" } else { "Godot pack(s)" };
            job.notes.push(format!("{n} {what} aren't taken apart for the inventory yet; their assets aren't listed."));
        }
        Ok(())
    }
}

// ---- Loose files ----

/// Pictures, sounds, models, fonts and videos the original keeps as plain files: copied as they are.
pub struct Loose;

impl OriginalReader for Loose {
    fn name(&self) -> &'static str {
        "Loose files"
    }

    fn read(&self, job: &mut Job<'_, '_>) -> Result<(), ToolError> {
        let files: Vec<(PathBuf, String, Kind)> = job
            .files
            .iter()
            .filter_map(|p| {
                let rel = relative(&job.game, p)?;
                let kind = classify(&rel)?;
                // An engine's own packages are read by their readers, not copied.
                let ext = rel.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
                if ENGINE_PACKAGES.contains(&ext.as_str()) {
                    return None;
                }
                matches!(kind, Kind::Texture | Kind::Sprite | Kind::Model | Kind::Sound | Kind::Music | Kind::Font | Kind::Video).then_some((
                    p.clone(),
                    rel,
                    kind,
                ))
            })
            .collect();
        for (n, (path, rel, kind)) in files.iter().enumerate() {
            job.ctx.cancel.check()?;
            if !job.has_room() {
                break;
            }
            if n % 100 == 0 {
                job.ctx.report(None, format!("Copying loose files: {n} of {}", files.len()));
            }
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let mut item = Item::new(Source::Original, rel, *kind, name.clone(), Status::Original);
            item.origin = Some(rel.clone());
            let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            item.bytes = size;
            let out = out_rel(*kind, "loose", &safe_path(rel, &safe_name(&name, 120)));
            let target = job.root.join(&out);
            if target.is_file() && std::fs::metadata(&target).is_ok_and(|m| m.len() == size) {
                item.path = Some(out);
                job.reuse(&mut item, &target);
            } else if size > MAX_LOOSE {
                item.problem = Some(format!("It's {size} bytes; loose files up to {MAX_LOOSE} bytes are copied."));
            } else if !job.fits(size) {
                job.over_budget(&mut item);
            } else {
                let made = target.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::copy(path, &target));
                match made {
                    Ok(copied) => {
                        item.path = Some(out);
                        job.bytes = job.bytes.saturating_add(copied);
                        job.taken_out += 1;
                        inventory::describe_file(&mut item, &target, job.thumbs);
                    }
                    Err(e) => item.problem = Some(format!("It couldn't be copied: {e}")),
                }
            }
            job.items.push(item);
        }
        Ok(())
    }
}
