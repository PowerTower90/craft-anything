//! Generating game assets on this computer's own graphics cards, for free: pictures with
//! Z-Image Turbo and 3D shapes with Hunyuan3D 2.1, each on a ComfyUI server (one per card, or the
//! same server for both), the background taken off a picture with rembg (BiRefNet), and a shape
//! finished in Blender (cleaned, decimated to a budget, sized, pivoted, textured from its
//! reference picture, exported as GLB and FBX with a preview). Sounds and music come from the
//! audio server ([`audio`]), voice lines from Qwen3-TTS in its own Python ([`voice`]).
//!
//! The helper scripts ship inside the app ([`BG_REMOVE_PY`], [`FINISH_MODEL_PY`],
//! [`voice::VOICE_LINE_PY`]) and are written next to each other on first use. Every wait has a deadline; a server that stops moving fails the
//! item with a plain message ([`comfy::Stall`]); a batch keeps going past one item's failure.

pub mod audio;
pub mod comfy;
pub mod voice;

pub use audio::SoundKind;
use comfy::{Comfy, Limits};
use craft_anything_evidence::{ErrorCode, ToolError};
use craft_anything_platform::cancel::CancelToken;
use craft_anything_platform::process::{Launch, Process};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The background-removal helper (Python, rembg).
pub const BG_REMOVE_PY: &str = include_str!("scripts/bg_remove.py");
/// The Blender finishing script.
pub const FINISH_MODEL_PY: &str = include_str!("scripts/finish_model.py");

/// How long one step may take.
const BG_REMOVE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const BLENDER_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// How long a started server has to answer.
const START_WAIT: Duration = Duration::from_secs(4 * 60);

/// Where everything is.
#[derive(Debug, Clone, Default)]
pub struct Setup {
    /// The picture server (`http://127.0.0.1:8188`).
    pub picture_url: String,
    /// The 3D server; the same as the picture server with one card.
    pub shape_url: String,
    /// The sound and music server; usually one of the two above (it starts with that one).
    pub audio_url: String,
    /// What starts each server, when it isn't running: a script (`.ps1`, `.bat`, `.cmd`, `.sh`) or a program.
    pub picture_start: Option<PathBuf>,
    pub shape_start: Option<PathBuf>,
    /// The Python that has rembg (background removal); without it, pictures keep their background.
    pub bg_python: Option<PathBuf>,
    /// Blender.
    pub blender: Option<PathBuf>,
    /// The Python that has Qwen3-TTS and Whisper (voice lines).
    pub voice_python: Option<PathBuf>,
    /// vgmstream's command-line decoder, for an original's sounds in a game's own format (FMOD
    /// banks and the like); none: looked for on the search path.
    pub sound_decoder: Option<PathBuf>,
    /// Where the helper scripts are written.
    pub scripts_dir: PathBuf,
    /// Waiting limits for the servers.
    pub limits: Limits,
}

impl Setup {
    pub fn picture(&self) -> Comfy {
        Comfy::new(&self.picture_url, "picture server")
    }

    pub fn shape(&self) -> Comfy {
        Comfy::new(&self.shape_url, "3D server")
    }

    pub fn audio(&self) -> Comfy {
        Comfy::new(&self.audio_url, "audio server")
    }

    /// What starts the audio server: the start command of the server it shares an address with.
    pub fn audio_start(&self) -> Option<&Path> {
        let base = self.audio().base().to_string();
        if base == self.shape().base() {
            self.shape_start.as_deref()
        } else if base == self.picture().base() {
            self.picture_start.as_deref()
        } else {
            None
        }
    }

    /// Write a helper script (when missing or changed); its path.
    fn write_script(&self, name: &str, text: &str) -> Result<PathBuf, ToolError> {
        std::fs::create_dir_all(&self.scripts_dir).map_err(|e| ToolError::io(self.scripts_dir.display(), e))?;
        let p = self.scripts_dir.join(name);
        if std::fs::read_to_string(&p).ok().as_deref() != Some(text) {
            std::fs::write(&p, text).map_err(|e| ToolError::io(p.display(), e))?;
        }
        Ok(p)
    }

    /// The background-removal and Blender scripts.
    fn scripts(&self) -> Result<(PathBuf, PathBuf), ToolError> {
        Ok((self.write_script("bg_remove.py", BG_REMOVE_PY)?, self.write_script("finish_model.py", FINISH_MODEL_PY)?))
    }
}

/// Blender: the configured one when it's there, else on the search path, else the usual install
/// places (the newest version).
pub fn find_blender(configured: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = configured.filter(|p| p.is_file()) {
        return Some(p.to_path_buf());
    }
    if let Some(p) = craft_anything_platform::which::which("blender") {
        return Some(p);
    }
    let mut found: Vec<PathBuf> = Vec::new();
    if cfg!(windows) {
        for base in [std::env::var_os("ProgramFiles"), std::env::var_os("ProgramW6432")].into_iter().flatten() {
            let dir = PathBuf::from(base).join("Blender Foundation");
            for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                let exe = e.path().join("blender.exe");
                if exe.is_file() {
                    found.push(exe);
                }
            }
        }
    } else if cfg!(target_os = "macos") {
        found.push(PathBuf::from("/Applications/Blender.app/Contents/MacOS/Blender"));
    } else {
        found.extend([PathBuf::from("/usr/bin/blender"), PathBuf::from("/snap/bin/blender")]);
    }
    found.retain(|p| p.is_file());
    found.sort();
    found.pop()
}

/// What one part looks like.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PartStatus {
    pub ready: bool,
    /// "ready", "not_running", "missing_model", "not_set_up".
    pub state: String,
    /// In words.
    pub detail: String,
    /// It isn't running but a start command is set.
    pub startable: bool,
}

/// What can generate right now.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub pictures: PartStatus,
    pub shapes: PartStatus,
    pub background_removal: PartStatus,
    pub blender: PartStatus,
    /// Sounds and music (the audio server).
    #[serde(default)]
    pub audio: PartStatus,
    /// Voice lines (the voice Python).
    #[serde(default)]
    pub voices: PartStatus,
    /// The decoder for an original's sounds (vgmstream).
    #[serde(default)]
    pub sound_decoder: PartStatus,
}

impl Status {
    /// Pictures can be made now, or once the servers are started.
    pub fn pictures_possible(&self) -> bool {
        self.pictures.ready || self.pictures.startable
    }

    /// 3D models can be made now, or once the servers are started (Blender is needed either way).
    pub fn models_possible(&self) -> bool {
        self.pictures_possible() && (self.shapes.ready || self.shapes.startable) && self.blender.ready
    }

    /// Sounds and music can be made now, or once the audio server is started.
    pub fn sounds_possible(&self) -> bool {
        self.audio.ready || self.audio.startable
    }
}

fn part(ready: bool, state: &str, detail: impl Into<String>, startable: bool) -> PartStatus {
    PartStatus { ready, state: state.into(), detail: detail.into(), startable }
}

fn server_status(server: &Comfy, start: Option<&Path>, need: &[(&str, &str)]) -> PartStatus {
    let startable = start.is_some_and(Path::is_file);
    if !server.reachable() {
        let how = if startable { "Start it here." } else { "Set its start command in Settings → Local generation, or start it yourself." };
        return part(false, "not_running", format!("Not running at {}. {how}", server.base()), startable);
    }
    let mut missing = Vec::new();
    for (folder, file) in need {
        match server.models(folder) {
            Ok(have) if have.iter().any(|h| h == file || h.ends_with(&format!("/{file}")) || h.ends_with(&format!("\\{file}"))) => {}
            Ok(_) => missing.push(format!("{file} (in models/{folder})")),
            Err(e) => missing.push(format!("{file} ({})", e.message)),
        }
    }
    if missing.is_empty() {
        part(true, "ready", format!("Ready at {}", server.base()), startable)
    } else {
        part(false, "missing_model", format!("Running at {}, but missing {}", server.base(), missing.join(", ")), startable)
    }
}

/// Look at every part: are the servers answering, do they have the model files, are Blender and
/// background removal there. Quick (a few short requests).
pub fn status(setup: &Setup) -> Status {
    let pictures = server_status(
        &setup.picture(),
        setup.picture_start.as_deref(),
        &[("diffusion_models", comfy::IMAGE_UNET), ("text_encoders", comfy::IMAGE_TEXT_ENCODER), ("vae", comfy::IMAGE_VAE)],
    );
    let shapes = server_status(&setup.shape(), setup.shape_start.as_deref(), &[("checkpoints", comfy::SHAPE_CHECKPOINT)]);
    let background_removal = match setup.bg_python.as_deref() {
        Some(p) if p.is_file() => part(true, "ready", format!("Python at {}", p.display()), false),
        Some(p) => part(false, "not_set_up", format!("{} isn't there.", p.display()), false),
        None => part(false, "not_set_up", "Not set up: pictures keep their white background.", false),
    };
    let blender = match find_blender(setup.blender.as_deref()) {
        Some(p) => part(true, "ready", format!("{}", p.display()), false),
        None => part(false, "not_set_up", "Blender wasn't found. Install it, or set its path in Settings → Local generation.", false),
    };
    let audio = server_status(
        &setup.audio(),
        setup.audio_start(),
        &[
            ("checkpoints", audio::SOUND_CHECKPOINT),
            ("text_encoders", audio::SOUND_TEXT_ENCODER),
            ("diffusion_models", audio::MUSIC_UNET),
            ("text_encoders", audio::MUSIC_TEXT_ENCODER_SMALL),
            ("text_encoders", audio::MUSIC_TEXT_ENCODER_LARGE),
            ("vae", audio::MUSIC_VAE),
        ],
    );
    let voices = match setup.voice_python.as_deref() {
        Some(p) if p.is_file() => part(true, "ready", format!("Python at {}", p.display()), false),
        Some(p) => part(false, "not_set_up", format!("{} isn't there.", p.display()), false),
        None => part(false, "not_set_up", "Not set up: set the Python that has Qwen3-TTS.", false),
    };
    let sound_decoder = match audio::find_decoder(setup.sound_decoder.as_deref()) {
        Some(p) => part(true, "ready", format!("{}", p.display()), false),
        None => part(false, "not_set_up", "vgmstream wasn't found: an original's sounds in a game's own format can't be listened to.", false),
    };
    Status { pictures, shapes, background_removal, blender, audio, voices, sound_decoder }
}

/// Start what isn't running (each start command once; a server shared by both parts once), then
/// wait until they answer. Returns what was started.
pub fn start(setup: &Setup, cancel: &CancelToken, progress: &dyn Fn(&str)) -> Result<Vec<String>, ToolError> {
    let mut started = Vec::new();
    let mut servers: Vec<(Comfy, Option<PathBuf>)> = vec![(setup.picture(), setup.picture_start.clone())];
    for (server, start) in [(setup.shape(), setup.shape_start.clone()), (setup.audio(), setup.audio_start().map(Path::to_path_buf))] {
        if !servers.iter().any(|(s, _)| s.base() == server.base()) {
            servers.push((server, start));
        }
    }
    for (server, start) in &servers {
        if server.reachable() {
            continue;
        }
        let Some(script) = start.as_deref().filter(|p| p.is_file()) else {
            return Err(ToolError::new(ErrorCode::EngineMissing, format!("The server at {} isn't running and has no start command.", server.base()))
                .with_remediation("Set its start command in Settings → Local generation, or start it yourself."));
        };
        progress(&format!("Starting {}", server.base()));
        launch_script(script)?;
        started.push(server.base().to_string());
    }
    for (server, _) in &servers {
        let t0 = Instant::now();
        while !server.reachable() {
            cancel.check()?;
            if t0.elapsed() >= START_WAIT {
                return Err(ToolError::new(
                    ErrorCode::TimedOut,
                    format!("The server at {} didn't answer within {} s of starting.", server.base(), START_WAIT.as_secs()),
                )
                .with_remediation("Look at its log; a first start can take long while it loads models."));
            }
            progress(&format!("Waiting for {} ({} s)", server.base(), t0.elapsed().as_secs()));
            std::thread::sleep(Duration::from_secs(2));
        }
    }
    Ok(started)
}

/// Start a server's start script and let it run on (a server outlives the call).
fn launch_script(script: &Path) -> Result<(), ToolError> {
    let ext = script.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    let launch = match ext.as_str() {
        "ps1" => Launch::new("powershell").args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]).arg(script),
        "bat" | "cmd" => Launch::new("cmd").arg("/C").arg(script),
        "sh" => Launch::new("sh").arg(script),
        _ => Launch::new(script),
    };
    launch.hide_console().label("the start command").start().map(|_| ())
}

/// A file name for an id: letters, digits, `-` and `_` only.
pub fn file_stem(id: &str) -> String {
    let s: String = id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).take(80).collect();
    if s.trim_matches('_').is_empty() { "item".into() } else { s }
}

/// Now, in nanoseconds since 1970 (for unique file names).
fn now_nanos() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
}

pub fn seed_for(id: &str) -> u64 {
    // FNV-1a: stable across runs, so the same id gives the same picture.
    id.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)) & 0x7fff_ffff_ffff
}

/// The prompt for a picture: a single subject on white (icons, sprites, model references) or a
/// whole picture (a texture, a background).
pub fn picture_prompt(description: &str, style: &str, single_object: bool) -> String {
    let style = style.trim().trim_end_matches('.');
    let style = if style.is_empty() { String::new() } else { format!(" {style}.") };
    if single_object {
        format!(
            "Game asset: {}.{style} Centered, single object, isolated on a plain white background, no text, no frame.",
            description.trim().trim_end_matches('.')
        )
    } else {
        format!("Game art: {}.{style} No text, no watermark.", description.trim().trim_end_matches('.'))
    }
}

/// The prompt for a 3D model's reference picture: the whole object, front-on, on white.
pub fn model_prompt(description: &str, style: &str) -> String {
    let style = style.trim().trim_end_matches('.');
    let style = if style.is_empty() { String::new() } else { format!(" {style}.") };
    format!(
        "3D game asset reference, front view: {}.{style} The whole object visible, centered, single object, even lighting, isolated on a plain white background, no text.",
        description.trim().trim_end_matches('.')
    )
}

/// Make one picture on the picture server, saved as `out` (a PNG).
#[allow(clippy::too_many_arguments)]
pub fn picture(
    setup: &Setup,
    prompt: &str,
    width: u32,
    height: u32,
    seed: u64,
    out: &Path,
    cancel: &CancelToken,
    progress: &dyn Fn(&str),
) -> Result<u64, ToolError> {
    let server = setup.picture();
    let prefix = out.file_stem().and_then(|s| s.to_str()).map(file_stem).unwrap_or_else(|| "picture".into());
    let id = server.queue(&comfy::image_graph(prompt, width.clamp(256, 2048) / 16 * 16, height.clamp(256, 2048) / 16 * 16, seed, &prefix))?;
    let outputs = server.wait(&id, setup.limits, cancel, progress)?;
    let file = comfy::first_output(&outputs, comfy::IMAGE_OUTPUT_NODE)
        .ok_or_else(|| ToolError::new(ErrorCode::GarbledOutput, "The picture server finished without a picture."))?;
    let bytes = server.view(&file)?;
    write(out, &bytes)?;
    Ok(bytes.len() as u64)
}

fn write(out: &Path, bytes: &[u8]) -> Result<(), ToolError> {
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir).map_err(|e| ToolError::io(dir.display(), e))?;
    }
    std::fs::write(out, bytes).map_err(|e| ToolError::io(out.display(), e))
}

/// Take the background off `src`: the subject alone (`out_rgba`) and on white (`out_white`).
pub fn remove_background(setup: &Setup, src: &Path, out_rgba: &Path, out_white: &Path, cancel: &CancelToken) -> Result<(), ToolError> {
    let python = setup.bg_python.as_deref().filter(|p| p.is_file()).ok_or_else(|| {
        ToolError::new(ErrorCode::EngineMissing, "Background removal isn't set up.")
            .with_remediation("Set the Python that has rembg in Settings → Local generation (pip install rembg).")
    })?;
    let (script, _) = setup.scripts()?;
    let c = cancel.clone();
    let out = Process::new(python)
        .arg(&script)
        .arg(src)
        .arg(out_rgba)
        .arg(out_white)
        .env("PYTHONUTF8", "1")
        .timeout(BG_REMOVE_TIMEOUT)
        .cancel_when(move || c.is_cancelled())
        .label("background removal")
        .run_checked()?;
    if !out.stdout_text().contains("BGREMOVE ok") || !out_rgba.is_file() {
        return Err(ToolError::new(ErrorCode::ProgramFailed, "Background removal didn't produce a picture.")
            .with_details(json!({"output": craft_anything_platform::process::output_excerpt(&out.stderr_text())})));
    }
    Ok(())
}

/// Finish a shape in Blender: `<out_base>.glb`, `.fbx` and `_preview.png`; Blender's report.
pub fn finish_model(
    setup: &Setup,
    shape: &Path,
    reference: &Path,
    out_base: &Path,
    faces: u32,
    height_m: f32,
    cancel: &CancelToken,
) -> Result<Value, ToolError> {
    let blender = find_blender(setup.blender.as_deref()).ok_or_else(|| {
        ToolError::new(ErrorCode::EngineMissing, "Blender wasn't found.").with_remediation("Install Blender, or set its path in Settings → Local generation.")
    })?;
    let (_, script) = setup.scripts()?;
    let c = cancel.clone();
    let out = Process::new(blender)
        .args(["-b", "--factory-startup", "--python"])
        .arg(&script)
        .arg("--")
        .arg(shape)
        .arg(reference)
        .arg(out_base)
        .arg(faces.clamp(500, 500_000).to_string())
        .arg(format!("{:.3}", height_m.clamp(0.01, 1000.0)))
        .timeout(BLENDER_TIMEOUT)
        .cancel_when(move || c.is_cancelled())
        .label("Blender")
        .run_checked()?;
    let text = out.stdout_text();
    let report = text.lines().rev().find_map(|l| l.strip_prefix("FINISH ")).and_then(|j| serde_json::from_str::<Value>(j).ok()).ok_or_else(|| {
        ToolError::new(ErrorCode::ProgramFailed, "Blender didn't finish the model.")
            .with_details(json!({"output": craft_anything_platform::process::output_excerpt(&format!("{text}\n{}", out.stderr_text()))}))
    })?;
    Ok(report)
}

/// What to make in a batch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    /// A picture (PNG).
    Image,
    /// A 3D model (GLB and FBX).
    Model,
    /// A sound effect, ambience or UI sound (WAV), on the audio server.
    Sound,
    /// A piece of music (WAV), on the audio server.
    Music,
    /// A character's spoken line (WAV), in the character's designed voice.
    Voice,
}

/// One thing to make.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BatchItem {
    /// Names the output files (an inventory item's id marks that item Generated).
    pub id: String,
    pub kind: ItemKind,
    /// What it shows, in plain words.
    pub description: String,
    /// For a picture: its size in pixels (default 1024×1024).
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    /// For a picture: a single subject with its background taken off (default true; false for a
    /// texture or background that fills the picture).
    #[serde(default)]
    pub transparent: Option<bool>,
    /// For a model: its height in metres (default 1).
    #[serde(default)]
    pub height_m: Option<f32>,
    /// For a model: the most faces (default 20000).
    #[serde(default)]
    pub faces: Option<u32>,
    /// For a sound or music: its length in seconds (keep it close to the original's; sounds
    /// 0.5–47, music 10–240).
    #[serde(default)]
    pub seconds: Option<f32>,
    /// For a sound: sfx (default), ambience or ui.
    #[serde(default)]
    pub sound: Option<SoundKind>,
    /// For a voice line: whose line it is (one voice per character).
    #[serde(default)]
    pub character: Option<String>,
    /// For a voice line: the character's voice in words (the same for all their lines).
    #[serde(default)]
    pub voice_description: Option<String>,
    /// For a voice line: the words. Without them, the original is transcribed (`transcribe_from`).
    #[serde(default)]
    pub text: Option<String>,
    /// For a voice line: the original recording, only to transcribe its words.
    #[serde(default)]
    pub transcribe_from: Option<PathBuf>,
}

impl BatchItem {
    /// An item of `kind` with only an id and a description.
    pub fn new(id: impl Into<String>, kind: ItemKind, description: impl Into<String>) -> Self {
        BatchItem {
            id: id.into(),
            kind,
            description: description.into(),
            width: None,
            height: None,
            transparent: None,
            height_m: None,
            faces: None,
            seconds: None,
            sound: None,
            character: None,
            voice_description: None,
            text: None,
            transcribe_from: None,
        }
    }
}

/// What became of one item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemResult {
    pub id: String,
    pub kind: ItemKind,
    pub ok: bool,
    /// The files made (the main one first).
    pub files: Vec<PathBuf>,
    pub error: Option<String>,
    pub seconds: f64,
    /// Blender's report for a model.
    #[serde(default)]
    pub stats: Value,
}

/// A model's reference picture, made and ready for the 3D server.
struct Reference {
    index: usize,
    item: BatchItem,
    raw: PathBuf,
    rgba: PathBuf,
    white: PathBuf,
    started: Instant,
}

/// Make one picture item: generate, then take the background off if asked (and possible).
fn picture_item(
    setup: &Setup,
    item: &BatchItem,
    style: &str,
    dir: &Path,
    stem: &str,
    cancel: &CancelToken,
    say: &dyn Fn(&str),
) -> Result<Vec<PathBuf>, ToolError> {
    let single = item.transparent.unwrap_or(true);
    let prompt = picture_prompt(&item.description, style, single);
    let out = dir.join(format!("{stem}.png"));
    let (w, h) = (item.width.unwrap_or(1024), item.height.unwrap_or(1024));
    if single && setup.bg_python.as_deref().is_some_and(Path::is_file) {
        let raw = dir.join(format!("{stem}_raw.png"));
        picture(setup, &prompt, w, h, seed_for(&item.id), &raw, cancel, say)?;
        say("taking the background off");
        let white = dir.join(format!("{stem}_white.png"));
        remove_background(setup, &raw, &out, &white, cancel)?;
        let _ = std::fs::remove_file(&white);
        Ok(vec![out, raw])
    } else {
        picture(setup, &prompt, w, h, seed_for(&item.id), &out, cancel, say)?;
        Ok(vec![out])
    }
}

/// A model's reference picture (square), with its background taken off when possible.
#[allow(clippy::too_many_arguments)]
fn reference_item(
    setup: &Setup,
    index: usize,
    item: &BatchItem,
    style: &str,
    dir: &Path,
    stem: &str,
    cancel: &CancelToken,
    say: &dyn Fn(&str),
) -> Result<Reference, ToolError> {
    let started = Instant::now();
    let raw = dir.join(format!("{stem}_ref_raw.png"));
    picture(setup, &model_prompt(&item.description, style), 1024, 1024, seed_for(&item.id), &raw, cancel, say)?;
    let (rgba, white) = if setup.bg_python.as_deref().is_some_and(Path::is_file) {
        say("taking the background off");
        let (rgba, white) = (dir.join(format!("{stem}_ref.png")), dir.join(format!("{stem}_ref_white.png")));
        remove_background(setup, &raw, &rgba, &white, cancel)?;
        (rgba, white)
    } else {
        (raw.clone(), raw.clone())
    };
    Ok(Reference { index, item: item.clone(), raw, rgba, white, started })
}

/// The shape and the finished model from a reference picture.
fn model_from(setup: &Setup, r: &Reference, dir: &Path, stem: &str, cancel: &CancelToken, say: &dyn Fn(&str)) -> Result<(Vec<PathBuf>, Value), ToolError> {
    let server = setup.shape();
    let bytes = std::fs::read(&r.white).map_err(|e| ToolError::io(r.white.display(), e))?;
    let name = server.upload_image(&bytes, &format!("craft_{stem}.png"))?;
    let id = server.queue(&comfy::shape_graph(&name, seed_for(&r.item.id), 30, 256, stem))?;
    let outputs = server.wait(&id, setup.limits, cancel, say)?;
    let file = comfy::first_output(&outputs, comfy::SHAPE_OUTPUT_NODE)
        .ok_or_else(|| ToolError::new(ErrorCode::GarbledOutput, "The 3D server finished without a shape."))?;
    let shape = dir.join(format!("{stem}_shape.glb"));
    write(&shape, &server.view(&file)?)?;
    say("finishing in Blender");
    let base = dir.join(stem);
    let stats = finish_model(setup, &shape, &r.rgba, &base, r.item.faces.unwrap_or(20_000), r.item.height_m.unwrap_or(1.0), cancel)?;
    let with = |ext: &str| PathBuf::from(format!("{}{ext}", base.display()));
    Ok((vec![with(".glb"), with(".fbx"), with("_preview.png"), r.rgba.clone(), r.raw.clone(), shape], stats))
}

/// A sound or music item: made on the audio server as `<stem>.wav`.
fn sound_item(
    setup: &Setup,
    item: &BatchItem,
    style: &str,
    dir: &Path,
    stem: &str,
    cancel: &CancelToken,
    say: &dyn Fn(&str),
) -> Result<Vec<PathBuf>, ToolError> {
    let kind = if item.kind == ItemKind::Music { SoundKind::Music } else { item.sound.filter(|k| *k != SoundKind::Music).unwrap_or(SoundKind::Sfx) };
    let out = dir.join(format!("{stem}.wav"));
    audio::sound(setup, kind, &item.description, style, item.seconds, seed_for(&item.id), &out, cancel, say)?;
    Ok(vec![out])
}

/// What one item made: its files (the main one first) and stats, or why not.
type Made = Result<(Vec<PathBuf>, Value), ToolError>;

/// Every voice item at once, in one run of the voice helper (its models load once).
fn voice_items(
    setup: &Setup,
    items: &[(usize, &BatchItem, String)],
    dir: &Path,
    voices_dir: &Path,
    cancel: &CancelToken,
    say: &dyn Fn(&str),
) -> Vec<(usize, Made)> {
    let mut lines = Vec::new();
    let mut out: Vec<(usize, Made)> = Vec::new();
    for (i, item, stem) in items {
        let character = item.character.as_deref().map(str::trim).unwrap_or_default();
        let has_words = item.text.as_deref().is_some_and(|t| !t.trim().is_empty()) || item.transcribe_from.is_some();
        if character.is_empty() || !has_words {
            out.push((*i, Err(ToolError::invalid("A voice line needs its character, and its words (text) or the original to transcribe (transcribe_from)."))));
            continue;
        }
        if voice::voice_of(voices_dir, character).is_none() && item.voice_description.as_deref().is_none_or(|d| d.trim().is_empty()) {
            out.push((*i, Err(ToolError::invalid(format!("{character} has no voice yet: describe it in voice_description.")))));
            continue;
        }
        lines.push((
            *i,
            voice::VoiceLine {
                id: format!("{i}"),
                character: character.to_string(),
                voice_description: item.voice_description.clone().unwrap_or_default(),
                text: item.text.clone(),
                transcribe_from: item.transcribe_from.clone(),
                out: dir.join(format!("{stem}.wav")),
            },
        ));
    }
    if lines.is_empty() {
        return out;
    }
    let only: Vec<voice::VoiceLine> = lines.iter().map(|(_, l)| l.clone()).collect();
    match voice::voice_lines(setup, voices_dir, &only, cancel, say) {
        Ok(results) => {
            for ((i, _), r) in lines.iter().zip(results) {
                let stats = json!({"text": r.text, "length_seconds": r.length, "voice": r.reference, "designed_voice": r.designed_voice});
                out.push((*i, if r.ok { Ok((vec![r.out], stats)) } else { Err(ToolError::new(ErrorCode::ProgramFailed, r.error.unwrap_or_default())) }));
            }
        }
        Err(e) => out.extend(lines.iter().map(|(i, _)| (*i, Err(e.clone())))),
    }
    out
}

/// Make every item, writing into `dir`: pictures one after another on the picture server; each
/// model's shape on the 3D server while the next pictures are made; sounds and music on the audio
/// server, and voice lines (their voices kept in `voices_dir`) on the voice helper, at the same
/// time. One item failing doesn't stop the rest. `progress` hears (done, total, what's happening).
#[allow(clippy::too_many_arguments)]
pub fn run_batch(
    setup: &Setup,
    items: &[BatchItem],
    style: &str,
    dir: &Path,
    voices_dir: &Path,
    cancel: &CancelToken,
    progress: &(dyn Fn(usize, usize, &str) + Sync),
) -> Vec<ItemResult> {
    let total = items.len();
    let mut stems: Vec<String> = Vec::new();
    for item in items {
        let mut s = file_stem(&item.id);
        let base = s.clone();
        let mut n = 2;
        while stems.contains(&s) {
            s = format!("{base}_{n}");
            n += 1;
        }
        stems.push(s);
    }
    let done = std::sync::atomic::AtomicUsize::new(0);
    let results = craft_anything_platform::sync::Mutex::new(vec![None::<ItemResult>; total]);
    let say = |i: usize, what: &str| {
        let label = items.get(i).map(|it| it.id.as_str()).unwrap_or_default();
        progress(done.load(std::sync::atomic::Ordering::Relaxed), total, &format!("{label}: {what}"));
    };
    let finish = |i: usize, kind: ItemKind, started: Instant, r: Result<(Vec<PathBuf>, Value), ToolError>| {
        let (ok, files, error, stats) = match r {
            Ok((files, stats)) => (true, files, None, stats),
            Err(e) => (false, Vec::new(), Some(e.for_people()), Value::Null),
        };
        let id = items.get(i).map(|it| it.id.clone()).unwrap_or_default();
        if let Some(slot) = results.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get_mut(i) {
            *slot = Some(ItemResult { id, kind, ok, files, error, seconds: started.elapsed().as_secs_f64(), stats });
        }
        let n = done.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        progress(n, total, &format!("{n} of {total} done"));
    };
    let (tx, rx) = std::sync::mpsc::channel::<Reference>();
    std::thread::scope(|scope| {
        // The 3D server works through the references as they come.
        scope.spawn(|| {
            for r in rx {
                let stem = stems.get(r.index).cloned().unwrap_or_default();
                let result = if cancel.is_cancelled() { Err(ToolError::cancelled()) } else { model_from(setup, &r, dir, &stem, cancel, &|w| say(r.index, w)) };
                finish(r.index, ItemKind::Model, r.started, result);
            }
        });
        // The audio server makes the sounds and music, one after another.
        scope.spawn(|| {
            for (i, item) in items.iter().enumerate().filter(|(_, it)| matches!(it.kind, ItemKind::Sound | ItemKind::Music)) {
                let started = Instant::now();
                let stem = stems.get(i).cloned().unwrap_or_default();
                let r = if cancel.is_cancelled() {
                    Err(ToolError::cancelled())
                } else {
                    say(i, if item.kind == ItemKind::Music { "making the music" } else { "making the sound" });
                    sound_item(setup, item, style, dir, &stem, cancel, &|w| say(i, w)).map(|f| (f, Value::Null))
                };
                finish(i, item.kind.clone(), started, r);
            }
        });
        // The voice helper speaks every line.
        scope.spawn(|| {
            let voiced: Vec<(usize, &BatchItem, String)> = items
                .iter()
                .enumerate()
                .filter(|(_, it)| it.kind == ItemKind::Voice)
                .map(|(i, it)| (i, it, stems.get(i).cloned().unwrap_or_default()))
                .collect();
            if voiced.is_empty() {
                return;
            }
            let started = Instant::now();
            let first = voiced[0].0;
            let results = if cancel.is_cancelled() {
                voiced.iter().map(|(i, _, _)| (*i, Err(ToolError::cancelled()))).collect()
            } else {
                voice_items(setup, &voiced, dir, voices_dir, cancel, &|w| say(first, w))
            };
            for (i, r) in results {
                finish(i, ItemKind::Voice, started, r);
            }
        });
        for (i, item) in items.iter().enumerate() {
            let stem = stems.get(i).cloned().unwrap_or_default();
            let started = Instant::now();
            if matches!(item.kind, ItemKind::Sound | ItemKind::Music | ItemKind::Voice) {
                continue;
            }
            if cancel.is_cancelled() {
                finish(i, item.kind.clone(), started, Err(ToolError::cancelled()));
                continue;
            }
            match item.kind {
                // Made in their own lanes.
                ItemKind::Sound | ItemKind::Music | ItemKind::Voice => {}
                ItemKind::Image => {
                    say(i, "making the picture");
                    let r = picture_item(setup, item, style, dir, &stem, cancel, &|w| say(i, w)).map(|f| (f, Value::Null));
                    finish(i, ItemKind::Image, started, r);
                }
                ItemKind::Model => {
                    say(i, "making the reference picture");
                    match reference_item(setup, i, item, style, dir, &stem, cancel, &|w| say(i, w)) {
                        Ok(r) => {
                            let _ = tx.send(r);
                        }
                        Err(e) => finish(i, ItemKind::Model, started, Err(e)),
                    }
                }
            }
        }
        drop(tx);
    });
    let results = results.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner);
    results
        .into_iter()
        .enumerate()
        .map(|(i, r)| {
            r.unwrap_or_else(|| ItemResult {
                id: items.get(i).map(|it| it.id.clone()).unwrap_or_default(),
                kind: items.get(i).map(|it| it.kind.clone()).unwrap_or(ItemKind::Image),
                ok: false,
                files: Vec::new(),
                error: Some("It wasn't made.".into()),
                seconds: 0.0,
                stats: Value::Null,
            })
        })
        .collect()
}

/// A batch's manifest: the job, its style, and every item with its files (relative to `root`
/// when they're inside it) or its error.
pub fn manifest(job: &str, style: &str, root: &Path, items: &[BatchItem], results: &[ItemResult]) -> Value {
    let rel = |p: &Path| p.strip_prefix(root).map(|r| r.to_string_lossy().replace('\\', "/")).unwrap_or_else(|_| p.display().to_string());
    let list: Vec<Value> = results
        .iter()
        .zip(items)
        .map(|(r, item)| {
            json!({
                "id": r.id, "kind": r.kind, "description": item.description, "ok": r.ok,
                "files": r.files.iter().map(|f| rel(f)).collect::<Vec<_>>(),
                "error": r.error, "seconds": (r.seconds * 10.0).round() / 10.0, "stats": r.stats,
            })
        })
        .collect();
    json!({
        "version": 1,
        "job": job,
        "style": style,
        "made": results.iter().filter(|r| r.ok).count(),
        "failed": results.iter().filter(|r| !r.ok).count(),
        "items": list,
    })
}

#[cfg(test)]
mod tests;
