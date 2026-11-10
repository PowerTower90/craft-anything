//! Talking with Craft Anything by voice: `voice_status`, `voice_start`, `voice_stop`,
//! `voice_speak`, `voice_config`, `voice_enroll` and `voice_design`. The service and the link to it are the engines layer's
//! ([`craft_anything_engines::voice`]); the running service lives in the session
//! ([`VoiceSlot`], in `Session::ext`), so the app, the conversation and these tools share one.
//! Starting waits for the service after the session is released, so other calls carry on.

use crate::session::Session;
use crate::tool::{Ctx, Effects, Progress, Requires, ToolOutput, ToolSpec, Toolset, parse_params, schema_of};
use craft_anything_engines::voice::{self, ListenMode, VoiceClient, VoiceEvent, VoiceSetup};
use craft_anything_evidence::{ErrorCode, ToolError};
use craft_anything_platform::cancel::CancelToken;
use craft_anything_platform::sync::Mutex;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

/// The folder under the config dir the service script (and, by default, its models) live in.
pub const VOICE_DIR: &str = "voice";
/// The longest text `voice_speak` takes.
const MAX_SPEAK_CHARS: usize = 5000;

pub fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            id: "voice_status",
            title: "Check voice",
            summary: "Whether Craft Anything can talk by voice on this computer, and whether it is: the voice Python (voice.python, else local_gen.voice_python), the voice service script, the port (free, a voice service, or another program), the speech models, whether this app's voice service is running (its process and last log lines), and the [voice] settings (listen mode, wake word, push-to-talk key, speaker threshold, task folder). Nothing is started.",
            beginner: "Checks whether you can talk to Craft Anything by voice.",
            toolset: Toolset::Setup,
            effects: Effects::ACCESSES_NETWORK.union(Effects::IDEMPOTENT),
            requires: Requires::Nothing,
            input_schema: schema_of::<NoParams>,
            run: voice_status,
        },
        ToolSpec {
            id: "voice_start",
            title: "Start voice",
            summary: "Start the local voice service (microphone, speech to text, speaker check and speech, all on this computer) with the voice Python, wait until it says it's ready (up to 3 minutes while its models load) and give it the [voice] settings. It runs until voice_stop or until Craft Anything closes. Does nothing when it is already running.",
            beginner: "Starts listening and speaking, so you can talk to Craft Anything.",
            toolset: Toolset::Setup,
            effects: Effects::LAUNCHES_PROCESS.union(Effects::ACCESSES_NETWORK).union(Effects::WRITES_FILESYSTEM).union(Effects::MUTATES_SESSION),
            requires: Requires::Nothing,
            input_schema: schema_of::<NoParams>,
            run: voice_start,
        },
        ToolSpec {
            id: "voice_stop",
            title: "Stop voice",
            summary: "Stop the voice service this app started: it is asked to end, then stopped if it doesn't. Does nothing when it isn't running.",
            beginner: "Stops listening and speaking.",
            toolset: Toolset::Setup,
            effects: Effects::ACCESSES_NETWORK.union(Effects::MUTATES_SESSION),
            requires: Requires::Nothing,
            input_schema: schema_of::<NoParams>,
            run: voice_stop,
        },
        ToolSpec {
            id: "voice_speak",
            title: "Speak aloud",
            summary: "Say `text` aloud in the assistant's voice through the running voice service (queued behind what is being said; the person talking over it stops it unless `interruptible` is false). With `wait`, returns once it has been said (or `timeout_s`, default 120), with whether it was interrupted. Needs voice_start first.",
            beginner: "Says something out loud.",
            toolset: Toolset::Setup,
            effects: Effects::ACCESSES_NETWORK,
            requires: Requires::Nothing,
            input_schema: schema_of::<SpeakParams>,
            run: voice_speak,
        },
        ToolSpec {
            id: "voice_enroll",
            title: "Enrol my voice",
            summary: "Record the person's voice for the speaker check, so only they can give instructions by voice: `action` start begins recording (they read a few sentences aloud, about 20 seconds in all), stop ends it and saves the profile when there was enough (returns enrolled true or false with the seconds heard). Needs voice_start first. Until a voice is enrolled, nothing said is sent to Opus.",
            beginner: "Teaches Craft Anything your voice, so only you can give it instructions.",
            toolset: Toolset::Setup,
            effects: Effects::ACCESSES_NETWORK.union(Effects::WRITES_FILESYSTEM),
            requires: Requires::Nothing,
            input_schema: schema_of::<EnrollParams>,
            run: voice_enroll,
        },
        ToolSpec {
            id: "voice_design",
            title: "Design the assistant's voice",
            summary: "Design a new voice for the assistant from a `description` (\"a calm, warm female voice, mid-thirties\") with Qwen3-TTS on this computer, saying `sample_text` (default: a short greeting); returns the reference clip and its text once made (up to `timeout_s`, default 300). Keep it with voice_config assistant_voice_ref_audio and assistant_voice_ref_text. Needs voice_start first.",
            beginner: "Makes a new voice for the assistant from a description.",
            toolset: Toolset::Setup,
            effects: Effects::ACCESSES_NETWORK.union(Effects::WRITES_FILESYSTEM),
            requires: Requires::Nothing,
            input_schema: schema_of::<DesignParams>,
            run: voice_design,
        },
        ToolSpec {
            id: "voice_config",
            title: "Set up voice",
            summary: "Show or change the [voice] settings: python, port, device (cuda:0 or cpu), listen_mode (push_to_talk, wake_word, always), wake_word, push_to_talk_key, speaker_threshold (0-1), input_device and output_device (empty: the system's), language, vad_silence_ms, assistant_voice_ref_audio and assistant_voice_ref_text, task_folder (empty: the home folder), models_dir, auto_start, sounds (short chimes). Saved to config.toml and given to the running service at once; python, port, device and models_dir take effect at the next start (restart_needed). With nothing given, shows them. Public actions are always confirmed: that can't be changed.",
            beginner: "Changes how voice listens and speaks.",
            toolset: Toolset::Setup,
            effects: Effects::WRITES_FILESYSTEM.union(Effects::MUTATES_SESSION).union(Effects::ACCESSES_NETWORK),
            requires: Requires::Nothing,
            input_schema: schema_of::<ConfigParams>,
            run: voice_config,
        },
    ]
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NoParams {}

/// The session's voice service, shared by the tools, the app and the conversation: put in
/// `Session::ext` by [`slot`].
#[derive(Debug, Default)]
pub struct VoiceSlot {
    client: Mutex<Option<Arc<VoiceClient>>>,
    starting: AtomicBool,
}

impl VoiceSlot {
    /// The running service, when there is one (a dead one is let go).
    pub fn client(&self) -> Option<Arc<VoiceClient>> {
        let mut c = self.client.lock().unwrap_or_else(PoisonError::into_inner);
        if c.as_ref().is_some_and(|c| !c.is_alive()) {
            // Shut down off this thread: it can wait a few seconds.
            if let Some(dead) = c.take() {
                let _ = std::thread::Builder::new().name("voice-stop".into()).spawn(move || dead.shutdown());
            }
        }
        c.clone()
    }

    /// Keep `client` as the running service; the one it replaces, if any.
    pub fn put(&self, client: Arc<VoiceClient>) -> Option<Arc<VoiceClient>> {
        self.client.lock().unwrap_or_else(PoisonError::into_inner).replace(client)
    }

    /// Let go of the running service (the caller shuts it down).
    pub fn take(&self) -> Option<Arc<VoiceClient>> {
        self.client.lock().unwrap_or_else(PoisonError::into_inner).take()
    }

    /// A start is under way.
    pub fn starting(&self) -> bool {
        self.starting.load(Ordering::SeqCst)
    }
}

/// The session's voice slot, made on first use.
pub fn slot(session: &mut Session) -> Arc<VoiceSlot> {
    if let Some(s) = session.ext.get::<VoiceSlot>() {
        return s;
    }
    let s = Arc::new(VoiceSlot::default());
    session.ext.insert(s.clone());
    s
}

/// The session's running voice service, when there is one.
pub fn running(session: &Session) -> Option<Arc<VoiceClient>> {
    session.ext.get::<VoiceSlot>().and_then(|s| s.client())
}

/// The voice Python: `voice.python`, else the voice-lines Python (`local_gen.voice_python`).
fn python_of(session: &Session) -> (Option<PathBuf>, Option<&'static str>) {
    let s = &session.settings;
    match (&s.voice.python, &s.local_gen.voice_python) {
        (Some(p), _) => (Some(p.clone()), Some("voice.python")),
        (None, Some(p)) => (Some(p.clone()), Some("local_gen.voice_python")),
        (None, None) => (None, None),
    }
}

/// Where everything is, from the session's settings.
pub fn setup_of(session: &Session) -> VoiceSetup {
    let v = &session.settings.voice;
    let dir = session.config_dir().join(VOICE_DIR);
    VoiceSetup {
        python: python_of(session).0,
        port: v.port,
        models_dir: v.models_dir.clone().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| dir.join("models")),
        device: v.device.clone(),
        scripts_dir: dir,
        ready_timeout: voice::DEFAULT_READY_TIMEOUT,
        allow_inject: false,
    }
}

fn settings_json(session: &Session) -> Value {
    let v = &session.settings.voice;
    let mut out = serde_json::to_value(v).unwrap_or(Value::Null);
    if let Some(o) = out.as_object_mut() {
        o.insert("task_folder_used".into(), json!(v.task_folder().display().to_string()));
        o.insert("python_from".into(), json!(python_of(session).1));
    }
    out
}

fn not_running() -> ToolError {
    ToolError::new(ErrorCode::EngineMissing, "Voice isn't running.").with_remediation("Start it with voice_start (Settings → Voice).")
}

fn voice_status(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let _: NoParams = parse_params(params)?;
    let setup = setup_of(ctx.session);
    let settings = settings_json(ctx.session);
    let slot = slot(ctx.session);
    ctx.after_unlocking(move |_| {
        let client = slot.client();
        let service = voice::status(&setup);
        Ok(ToolOutput::Value(json!({
            "running": client.is_some(),
            "starting": slot.starting(),
            "pid": client.as_ref().and_then(|c| c.pid()),
            "port": client.as_ref().map_or(setup.port, |c| c.port()),
            "log": client.as_ref().map(|c| c.log_tail(20)).unwrap_or_default(),
            "service": service,
            "settings": settings,
        })))
    });
    Ok(ToolOutput::Value(Value::Null))
}

/// Clears the "starting" mark however the start ends.
struct Starting(Arc<VoiceSlot>);

impl Drop for Starting {
    fn drop(&mut self) {
        self.0.starting.store(false, Ordering::SeqCst);
    }
}

fn voice_start(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let _: NoParams = parse_params(params)?;
    let slot = slot(ctx.session);
    if let Some(c) = slot.client() {
        return Ok(ToolOutput::Value(json!({"started": false, "already_running": true, "pid": c.pid(), "port": c.port()})));
    }
    if slot.starting.swap(true, Ordering::SeqCst) {
        return Err(ToolError::new(ErrorCode::BadInput, "Voice is already starting.").with_remediation("Wait for it, then check with voice_status."));
    }
    let guard = Starting(slot.clone());
    let setup = setup_of(ctx.session);
    let config = ctx.session.settings.voice.service_config();
    ctx.after_unlocking_reporting(move |cancel, progress| {
        let _guard = guard;
        progress(Progress { fraction: None, message: "Starting the voice service".into() });
        let client = VoiceClient::start_cancellable(&setup, &cancel)?;
        client.config(&config)?;
        let (pid, port) = (client.pid(), client.port());
        if let Some(old) = slot.put(Arc::new(client)) {
            old.shutdown();
        }
        Ok(ToolOutput::Value(json!({"started": true, "pid": pid, "port": port, "placeholder": voice::service_is_placeholder()})))
    });
    Ok(ToolOutput::Value(Value::Null))
}

fn voice_stop(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let _: NoParams = parse_params(params)?;
    let Some(client) = slot(ctx.session).take() else {
        return Ok(ToolOutput::Value(json!({"stopped": false, "running": false})));
    };
    ctx.after_unlocking(move |_| {
        client.shutdown();
        Ok(ToolOutput::Value(json!({"stopped": true, "running": false})))
    });
    Ok(ToolOutput::Value(Value::Null))
}

fn d_true() -> bool {
    true
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SpeakParams {
    /// What to say (1 to 5000 characters).
    pub text: String,
    /// Names this utterance in the spoken event (default: made up).
    #[serde(default)]
    pub id: Option<String>,
    /// The person talking over it stops it (default true).
    #[serde(default = "d_true")]
    pub interruptible: bool,
    /// Return only once it has been said (default false: return when queued).
    #[serde(default)]
    pub wait: bool,
    /// With `wait`, the longest to wait in seconds (default 120, at most 600).
    #[serde(default)]
    pub timeout_s: Option<u64>,
}

fn voice_speak(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let p: SpeakParams = parse_params(params)?;
    let text = p.text.trim().to_string();
    if text.is_empty() || text.chars().count() > MAX_SPEAK_CHARS {
        return Err(ToolError::invalid(format!("Give the text to say (1 to {MAX_SPEAK_CHARS} characters).")));
    }
    let client = running(ctx.session).ok_or_else(not_running)?;
    let id =
        p.id.map(|i| i.trim().chars().take(64).collect::<String>())
            .filter(|i| !i.is_empty())
            .unwrap_or_else(|| format!("speak-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)));
    let events = p.wait.then(|| client.events());
    client.send(voice::VoiceCommand::Speak { id: id.clone(), text, interruptible: p.interruptible })?;
    let Some(events) = events else {
        return Ok(ToolOutput::Value(json!({"id": id, "queued": true})));
    };
    let timeout = Duration::from_secs(p.timeout_s.unwrap_or(120).clamp(1, 600));
    ctx.after_unlocking(move |cancel| wait_spoken(&events, &id, timeout, &cancel));
    Ok(ToolOutput::Value(Value::Null))
}

/// Start or stop recording the person's voice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EnrollAction {
    Start,
    Stop,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EnrollParams {
    pub action: EnrollAction,
}

fn voice_enroll(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let p: EnrollParams = parse_params(params)?;
    let client = running(ctx.session).ok_or_else(not_running)?;
    if p.action == EnrollAction::Start {
        client.enroll_start()?;
        return Ok(ToolOutput::Value(json!({"recording": true, "needed_seconds": 20})));
    }
    let events = client.events();
    client.enroll_stop()?;
    ctx.after_unlocking(move |cancel| {
        let t0 = Instant::now();
        let mut seconds = 0.0f32;
        while t0.elapsed() < Duration::from_secs(60) {
            cancel.check()?;
            match events.recv_timeout(Duration::from_millis(100)) {
                Ok(VoiceEvent::EnrollProgress { seconds: s, .. }) => seconds = s,
                Ok(VoiceEvent::Enrolled { ok }) => return Ok(ToolOutput::Value(json!({"enrolled": ok, "seconds": seconds}))),
                Ok(VoiceEvent::Disconnected) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                _ => {}
            }
        }
        Err(ToolError::new(ErrorCode::TimedOut, "The voice service didn't say whether the voice was enrolled."))
    });
    Ok(ToolOutput::Value(Value::Null))
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DesignParams {
    /// The voice in words: age, tone, pace, accent.
    pub description: String,
    /// What the new voice says in its reference clip.
    #[serde(default)]
    pub sample_text: Option<String>,
    /// How long to wait for it (10-900 s, default 300).
    #[serde(default)]
    pub timeout_s: Option<u64>,
}

fn voice_design(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let p: DesignParams = parse_params(params)?;
    let description = p.description.trim().to_string();
    if description.is_empty() || description.chars().count() > 1000 {
        return Err(ToolError::invalid("Describe the voice (1 to 1000 characters)."));
    }
    let sample = p.sample_text.map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).unwrap_or_else(|| "Hello! I'm here when you need me.".into());
    let client = running(ctx.session).ok_or_else(not_running)?;
    let events = client.events();
    client.design_voice(&description, &sample)?;
    let timeout = Duration::from_secs(p.timeout_s.unwrap_or(300).clamp(10, 900));
    ctx.after_unlocking(move |cancel| {
        let t0 = Instant::now();
        while t0.elapsed() < timeout {
            cancel.check()?;
            match events.recv_timeout(Duration::from_millis(100)) {
                Ok(VoiceEvent::VoiceDesigned { ref_audio, ref_text }) => {
                    return Ok(ToolOutput::Value(json!({"ref_audio": ref_audio.display().to_string(), "ref_text": ref_text})));
                }
                Ok(VoiceEvent::Error { message, .. }) if message.to_ascii_lowercase().contains("design") => {
                    return Err(ToolError::new(ErrorCode::ProgramFailed, message));
                }
                Ok(VoiceEvent::Disconnected) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(ToolError::new(ErrorCode::EngineMissing, "The voice service went away while designing the voice."));
                }
                _ => {}
            }
        }
        Err(ToolError::new(ErrorCode::TimedOut, format!("The voice wasn't designed within {} s.", timeout.as_secs())))
    });
    Ok(ToolOutput::Value(Value::Null))
}

fn wait_spoken(events: &std::sync::mpsc::Receiver<VoiceEvent>, id: &str, timeout: Duration, cancel: &CancelToken) -> Result<ToolOutput, ToolError> {
    let t0 = Instant::now();
    while t0.elapsed() < timeout {
        cancel.check()?;
        match events.recv_timeout(Duration::from_millis(100)) {
            Ok(VoiceEvent::Spoken { id: done, interrupted }) if done == id => {
                return Ok(ToolOutput::Value(json!({"id": id, "spoken": true, "interrupted": interrupted})));
            }
            Ok(VoiceEvent::Disconnected) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(ToolError::new(ErrorCode::EngineMissing, "The voice service went away before it finished speaking."));
            }
            _ => {}
        }
    }
    Err(ToolError::new(ErrorCode::TimedOut, format!("It wasn't finished saying it within {} s.", timeout.as_secs())))
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigParams {
    /// The Python that has the voice service's packages (empty: none).
    #[serde(default)]
    pub python: Option<PathBuf>,
    /// The loopback port (1024-65535).
    #[serde(default)]
    pub port: Option<u16>,
    /// `cuda:0` (the first graphics card), `cuda:1`… or `cpu`.
    #[serde(default)]
    pub device: Option<String>,
    #[serde(default)]
    pub listen_mode: Option<ListenMode>,
    #[serde(default)]
    pub wake_word: Option<String>,
    /// A key name such as F9.
    #[serde(default)]
    pub push_to_talk_key: Option<String>,
    /// 0 to 1: how alike an utterance must be to the enrolled voice.
    #[serde(default)]
    pub speaker_threshold: Option<f32>,
    /// A microphone by name (empty: the system's default).
    #[serde(default)]
    pub input_device: Option<String>,
    /// Speakers by name (empty: the system's default).
    #[serde(default)]
    pub output_device: Option<String>,
    /// The spoken language, e.g. en.
    #[serde(default)]
    pub language: Option<String>,
    /// Silence that ends an utterance, 100-5000 ms.
    #[serde(default)]
    pub vad_silence_ms: Option<u32>,
    /// The assistant's voice clip (empty: the service's own voice).
    #[serde(default)]
    pub assistant_voice_ref_audio: Option<PathBuf>,
    /// What the clip says.
    #[serde(default)]
    pub assistant_voice_ref_text: Option<String>,
    /// The working folder for tasks given by voice (empty: the home folder).
    #[serde(default)]
    pub task_folder: Option<PathBuf>,
    /// Where the speech models are kept (empty: in the settings folder).
    #[serde(default)]
    pub models_dir: Option<PathBuf>,
    /// Start voice when the app starts.
    #[serde(default)]
    pub auto_start: Option<bool>,
    /// Short chimes when listening starts and stops, a job is done, or something waits for an answer.
    #[serde(default)]
    pub sounds: Option<bool>,
}

/// A path setting: empty clears it; otherwise it must be a full path.
fn path_setting(name: &str, p: PathBuf) -> Result<Option<PathBuf>, ToolError> {
    if p.as_os_str().is_empty() {
        return Ok(None);
    }
    if !p.is_absolute() {
        return Err(ToolError::invalid(format!("{name} must be a full path.")));
    }
    Ok(Some(p))
}

fn text_setting(name: &str, s: &str, max: usize) -> Result<String, ToolError> {
    let s = s.trim();
    if s.chars().count() > max || s.chars().any(char::is_control) {
        return Err(ToolError::invalid(format!("{name} must be at most {max} characters on one line.")));
    }
    Ok(s.to_string())
}

fn voice_config(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let p: ConfigParams = parse_params(params)?;
    let before = ctx.session.settings.voice.clone();
    let mut v = before.clone();
    if let Some(x) = p.python {
        v.python = path_setting("python", x)?;
    }
    if let Some(x) = p.port {
        if x < 1024 {
            return Err(ToolError::invalid("port must be 1024 to 65535."));
        }
        v.port = x;
    }
    if let Some(x) = p.device {
        let x = text_setting("device", &x, 32)?;
        if !(x == "cpu"
            || x.strip_prefix("cuda")
                .is_some_and(|n| n.is_empty() || n.strip_prefix(':').is_some_and(|d| !d.is_empty() && d.chars().all(|c| c.is_ascii_digit()))))
        {
            return Err(ToolError::invalid("device is cuda:0 (a graphics card by number) or cpu."));
        }
        v.device = x;
    }
    if let Some(x) = p.listen_mode {
        v.listen_mode = x;
    }
    if let Some(x) = p.wake_word {
        v.wake_word = text_setting("wake_word", &x, 64)?;
        if v.wake_word.is_empty() {
            return Err(ToolError::invalid("wake_word can't be empty."));
        }
    }
    if let Some(x) = p.push_to_talk_key {
        v.push_to_talk_key = text_setting("push_to_talk_key", &x, 32)?;
        if v.push_to_talk_key.is_empty() {
            return Err(ToolError::invalid("push_to_talk_key can't be empty."));
        }
    }
    if let Some(x) = p.speaker_threshold {
        if !(0.0..=1.0).contains(&x) {
            return Err(ToolError::invalid("speaker_threshold is 0 to 1."));
        }
        v.speaker_threshold = x;
    }
    if let Some(x) = p.input_device {
        v.input_device = Some(text_setting("input_device", &x, 200)?).filter(|d| !d.is_empty());
    }
    if let Some(x) = p.output_device {
        v.output_device = Some(text_setting("output_device", &x, 200)?).filter(|d| !d.is_empty());
    }
    if let Some(x) = p.language {
        let x = text_setting("language", &x, 16)?;
        v.language = if x.is_empty() { "en".into() } else { x };
    }
    if let Some(x) = p.vad_silence_ms {
        if !(100..=5000).contains(&x) {
            return Err(ToolError::invalid("vad_silence_ms is 100 to 5000."));
        }
        v.vad_silence_ms = x;
    }
    if let Some(x) = p.assistant_voice_ref_audio {
        v.assistant_voice_ref_audio = path_setting("assistant_voice_ref_audio", x)?;
    }
    if let Some(x) = p.assistant_voice_ref_text {
        v.assistant_voice_ref_text = text_setting("assistant_voice_ref_text", &x, 2000)?;
    }
    if let Some(x) = p.task_folder {
        v.task_folder = path_setting("task_folder", x)?;
    }
    if let Some(x) = p.models_dir {
        v.models_dir = path_setting("models_dir", x)?;
    }
    if let Some(x) = p.auto_start {
        v.auto_start = x;
    }
    if let Some(x) = p.sounds {
        v.sounds = x;
    }
    v.confirm_public_actions = true;
    if v == before {
        return Ok(ToolOutput::Value(json!({"changed": false, "voice": settings_json(ctx.session)})));
    }
    let mut next = ctx.session.settings.clone();
    next.voice = v;
    let path = ctx.session.settings_path();
    next.save_to(&path)?;
    ctx.session.settings = next;
    let after = &ctx.session.settings.voice;
    let restart_needed = (after.python != before.python || after.port != before.port || after.device != before.device || after.models_dir != before.models_dir)
        && running(ctx.session).is_some();
    let applied = match running(ctx.session) {
        Some(c) => c.config(&after.service_config()).is_ok(),
        None => false,
    };
    Ok(ToolOutput::Value(json!({
        "changed": true,
        "path": path.display().to_string(),
        "applied_to_running_service": applied,
        "restart_needed": restart_needed,
        "voice": settings_json(ctx.session),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CallOptions, Caller, Engine};
    use craft_anything_engines::voice::standin::StandInService;

    fn call(e: &Engine, id: &str, p: Value) -> Result<Value, ToolError> {
        e.call(id, p, CallOptions::new(Caller::Mcp)).map(|o| o.to_json()["result"].clone())
    }

    #[test]
    fn without_a_voice_python_nothing_starts_and_it_says_why() {
        let e = Engine::new(crate::testing::session());
        let st = call(&e, "voice_status", json!({})).unwrap();
        assert_eq!(st["running"], false);
        assert_eq!(st["service"]["python_found"], false, "{st}");
        assert_eq!(st["service"]["startable"], false);
        assert_eq!(st["settings"]["confirm_public_actions"], true);
        assert_eq!(st["settings"]["push_to_talk_key"], "F9");
        let err = call(&e, "voice_start", json!({})).unwrap_err();
        assert_eq!(err.code, ErrorCode::EngineMissing, "{err:?}");
        assert!(err.remediation.unwrap_or_default().contains("voice.python"));
        // Starting again isn't blocked by the failed start.
        assert_eq!(call(&e, "voice_start", json!({})).unwrap_err().code, ErrorCode::EngineMissing);
        assert_eq!(call(&e, "voice_speak", json!({"text": "Hello."})).unwrap_err().code, ErrorCode::EngineMissing);
        assert_eq!(call(&e, "voice_stop", json!({})).unwrap(), json!({"stopped": false, "running": false}));
        assert_eq!(call(&e, "voice_speak", json!({"text": "  "})).unwrap_err().code, ErrorCode::BadInput);
    }

    #[test]
    fn the_settings_are_changed_checked_and_saved() {
        let s = crate::testing::session();
        let config = s.settings_path();
        let e = Engine::new(s);
        let r = call(&e, "voice_config", json!({})).unwrap();
        assert_eq!(r["changed"], false);
        let r = call(
            &e,
            "voice_config",
            json!({"listen_mode": "wake_word", "speaker_threshold": 0.7, "push_to_talk_key": "F10", "device": "cpu", "input_device": "USB mic"}),
        )
        .unwrap();
        assert_eq!((&r["changed"], &r["restart_needed"], &r["applied_to_running_service"]), (&json!(true), &json!(false), &json!(false)), "{r}");
        assert_eq!(r["voice"]["listen_mode"], "wake_word");
        let saved = crate::settings::Settings::load_from(&config);
        assert_eq!(
            (saved.voice.listen_mode, saved.voice.push_to_talk_key.as_str(), saved.voice.input_device.as_deref()),
            (ListenMode::WakeWord, "F10", Some("USB mic"))
        );
        assert!((saved.voice.speaker_threshold - 0.7).abs() < 1e-6);
        // Empty clears.
        call(&e, "voice_config", json!({"input_device": ""})).unwrap();
        assert_eq!(crate::settings::Settings::load_from(&config).voice.input_device, None);
        // Refused before anything is written.
        for bad in [
            json!({"speaker_threshold": 1.5}),
            json!({"port": 80}),
            json!({"device": "gpu"}),
            json!({"vad_silence_ms": 5}),
            json!({"task_folder": "relative/folder"}),
            json!({"wake_word": ""}),
            json!({"confirm_public_actions": false}),
            json!({"listen_mode": "sometimes"}),
        ] {
            assert_eq!(call(&e, "voice_config", bad.clone()).unwrap_err().code, ErrorCode::BadInput, "{bad}");
        }
        assert!(crate::settings::Settings::load_from(&config).voice.confirm_public_actions);
    }

    #[test]
    fn speaking_goes_through_the_running_service() {
        let svc = StandInService::start(Vec::new()).unwrap();
        let mut s = crate::testing::session();
        let slot = slot(&mut s);
        slot.put(Arc::new(VoiceClient::connect(svc.port()).unwrap()));
        let e = Engine::new(s);
        let st = call(&e, "voice_status", json!({})).unwrap();
        assert_eq!(st["running"], true, "{st}");
        assert_eq!(call(&e, "voice_start", json!({})).unwrap()["already_running"], true);
        let r = call(&e, "voice_speak", json!({"text": "Done. The trailer is ready.", "id": "t1", "wait": true, "timeout_s": 10})).unwrap();
        assert_eq!(r, json!({"id": "t1", "spoken": true, "interrupted": false}));
        let r = call(&e, "voice_speak", json!({"text": "Queued only."})).unwrap();
        assert_eq!(r["queued"], true);
        // A settings change reaches the running service.
        let r = call(&e, "voice_config", json!({"listen_mode": "always", "port": 9999})).unwrap();
        assert_eq!((&r["applied_to_running_service"], &r["restart_needed"]), (&json!(true), &json!(true)), "{r}");
        assert!(svc.wait_for_command("config", Duration::from_secs(5)));
        assert_eq!(svc.commands().iter().find(|c| c["cmd"] == "config").map(|c| c["listen_mode"].clone()), Some(json!("always")));
        let speak = svc.commands().into_iter().find(|c| c["cmd"] == "speak").unwrap();
        assert_eq!(speak, json!({"cmd": "speak", "id": "t1", "text": "Done. The trailer is ready.", "interruptible": true}));
        // Stopping asks the service to end and lets it go.
        assert_eq!(call(&e, "voice_stop", json!({})).unwrap()["stopped"], true);
        assert!(svc.shutdown_asked());
        assert_eq!(call(&e, "voice_status", json!({})).unwrap()["running"], false);
        assert_eq!(call(&e, "voice_speak", json!({"text": "Gone."})).unwrap_err().code, ErrorCode::EngineMissing);
    }
}
