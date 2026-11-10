//! The app's side of the voice service: starting it, the WebSocket to it, the commands sent and
//! the events heard.
//!
//! One thread per client owns the WebSocket: it sends the queued commands, reads the service's
//! events and hands each to every subscriber ([`VoiceClient::events`]). When the link drops while
//! the service still runs, the thread connects again (for [`RECONNECT_WINDOW`]), sends the last
//! configuration and asks for the state; only when it can't, or the service has ended, does every
//! subscriber get [`VoiceEvent::Disconnected`], once, and the client is dead.

use super::{PROTOCOL_VERSION, ServiceCommand, VoiceSetup, parse_ready, port_state};
use crate::cdp::{Deadline, WsConn};
use craft_anything_evidence::{ErrorCode, ToolError};
use craft_anything_platform::cancel::CancelToken;
use craft_anything_platform::process::{Process, RunningProcess};
use craft_anything_platform::sync::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How long the link may be down, with the service still running, before the client gives up.
pub const RECONNECT_WINDOW: Duration = Duration::from_secs(10);
/// Events a subscriber may fall behind by before new ones are dropped for it.
pub const SUBSCRIBER_BACKLOG: usize = 4096;
/// Events kept for the first subscriber from before it subscribed.
const EARLY_EVENTS: usize = 256;
/// The largest message taken from the service.
const MAX_MESSAGE: usize = 4 << 20;
/// How long one read waits before the thread looks at its queue again (the command latency).
const READ_SLICE: Duration = Duration::from_millis(20);
/// How long a started service has, after `READY`, to accept the WebSocket.
const CONNECT_WAIT: Duration = Duration::from_secs(5);
/// How long `shutdown` waits for the service to end by itself before it's killed.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(3);
/// The service's log lines kept for status and errors.
const LOG_LINES: usize = 200;
/// The service may run as long as the app does.
const SERVICE_LIFETIME: Duration = Duration::from_secs(400 * 24 * 60 * 60);

/// How the service listens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ListenMode {
    /// Only while the push-to-talk key is held.
    #[default]
    PushToTalk,
    /// After the wake word.
    WakeWord,
    /// Always (every finished utterance).
    Always,
}

impl ListenMode {
    pub fn id(self) -> &'static str {
        match self {
            ListenMode::PushToTalk => "push_to_talk",
            ListenMode::WakeWord => "wake_word",
            ListenMode::Always => "always",
        }
    }
}

/// The assistant's voice: a reference clip and what it says.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TtsVoice {
    pub ref_audio: PathBuf,
    pub ref_text: String,
}

/// The settings the service applies (`{"cmd":"config",…}`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VoiceConfig {
    pub listen_mode: ListenMode,
    pub wake_word: String,
    /// None: the system's default microphone.
    pub input_device: Option<String>,
    /// None: the system's default speakers.
    pub output_device: Option<String>,
    /// Utterances scoring below this against the enrolled voice are ignored (0–1).
    pub speaker_threshold: f32,
    /// The spoken language (`en`).
    pub language: String,
    /// None: the service's own default voice.
    pub tts_voice: Option<TtsVoice>,
    /// Silence that ends an utterance (ms).
    pub vad_silence_ms: u32,
    /// Short chimes when capturing starts and ends (and for [`VoiceCommand::Chime`]).
    #[serde(default = "sounds_on")]
    pub sounds: bool,
    /// Take the speakers' sound out of the microphone (WebRTC echo cancellation, when the
    /// service has it).
    #[serde(default = "sounds_on")]
    pub echo_cancellation: bool,
}

fn sounds_on() -> bool {
    true
}

impl Default for VoiceConfig {
    fn default() -> Self {
        VoiceConfig {
            listen_mode: ListenMode::PushToTalk,
            wake_word: "hey craft".into(),
            input_device: None,
            output_device: None,
            speaker_threshold: 0.55,
            language: "en".into(),
            tts_voice: None,
            vad_silence_ms: 700,
            sounds: true,
            echo_cancellation: true,
        }
    }
}

/// A command to the service, serialised as `{"cmd":"<snake_case>",…}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum VoiceCommand {
    /// Apply settings; answered with [`VoiceEvent::ConfigOk`].
    Config(VoiceConfig),
    /// The push-to-talk key: down starts capturing, up ends the utterance.
    Ptt {
        down: bool,
    },
    /// Speak `text`, sentence by sentence (queued behind what's being said); [`VoiceEvent::Spoken`]
    /// with this `id` when done.
    Speak {
        id: String,
        text: String,
        interruptible: bool,
    },
    /// Stop speaking now and clear the queue.
    StopSpeaking,
    /// Start or stop recording the person's voice for their speaker profile.
    EnrollStart,
    EnrollStop,
    /// Design a new assistant voice; answered with [`VoiceEvent::VoiceDesigned`].
    DesignVoice {
        description: String,
        sample_text: String,
    },
    /// List the audio devices; answered with [`VoiceEvent::Devices`].
    Devices,
    /// Ask for the state; answered with [`VoiceEvent::Status`].
    Status,
    /// End the service.
    Shutdown,
    /// A short, quiet tone: `done` (a job finished) or `ask` (something waits for an answer);
    /// `start` and `end` are played by the service itself around each capture.
    Chime {
        kind: String,
    },
    /// Tests only: feed a WAV in as if spoken into the microphone (`ptt`: hold push-to-talk around
    /// it; none: the service decides). Refused unless the service was started with
    /// [`super::VoiceSetup::allow_inject`]; answered with [`VoiceEvent::Injected`].
    InjectWav {
        path: PathBuf,
        #[serde(skip_serializing_if = "Option::is_none")]
        ptt: Option<bool>,
    },
}

impl VoiceCommand {
    /// The JSON text frame.
    pub fn to_json(&self) -> Result<String, ToolError> {
        serde_json::to_string(self).map_err(|e| ToolError::internal(format!("a voice command couldn't be written: {e}")))
    }
}

/// Something the service said, or that the link to it is gone.
///
/// Parsed leniently ([`VoiceEvent::parse`]): unknown fields are ignored, missing ones get their
/// empty value, and an event of a type this app doesn't know is skipped. Besides the contract's
/// events it has [`VoiceEvent::ConfigOk`] and [`VoiceEvent::Status`] (both in the protocol), and
/// it is `non_exhaustive` so later events don't break a `match`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum VoiceEvent {
    State {
        listening: bool,
        capturing: bool,
        speaking: bool,
        mode: String,
        enrolled: bool,
        /// The speech models are loaded (a service that doesn't say counts as loaded).
        models_ready: bool,
        /// What is loading, in words ("speech recognition"); empty when nothing is.
        loading: String,
        /// How far loading has got: `load_step` of `load_steps`.
        load_step: u32,
        load_steps: u32,
    },
    /// How loud the microphone (`input`) and the speakers (`output`) are, 0–1, about 20 times a
    /// second while there is sound.
    Level {
        input: f32,
        output: f32,
    },
    Wake {
        word: String,
        score: f32,
    },
    /// The words heard so far while capturing.
    Partial {
        text: String,
    },
    /// A finished utterance; `speaker_ok` says it's the enrolled person's voice. `verified` says the
    /// check was made against an enrolled voice: before enrolment every voice is `speaker_ok`, and
    /// `verified` is false (a service that doesn't say counts as verified).
    Utterance {
        text: String,
        speaker_ok: bool,
        speaker_score: f32,
        duration_s: f32,
        verified: bool,
    },
    /// The person spoke while the service was speaking: playback stopped, queue cleared.
    BargeIn,
    Spoken {
        id: String,
        interrupted: bool,
    },
    EnrollProgress {
        seconds: f32,
        needed: f32,
    },
    Enrolled {
        ok: bool,
    },
    VoiceDesigned {
        ref_audio: PathBuf,
        ref_text: String,
    },
    Devices {
        inputs: Vec<String>,
        outputs: Vec<String>,
    },
    Error {
        message: String,
        fatal: bool,
    },
    /// The configuration was applied.
    ConfigOk,
    /// The answer to [`VoiceCommand::Status`]: the event's other fields.
    Status {
        state: Value,
    },
    /// Something heard was the assistant's own speech coming back (its words, or its voice), and
    /// was dropped: `reason` is `own_words` or `own_voice`.
    EchoIgnored {
        text: String,
        reason: String,
    },
    /// Tests only: a WAV given with [`VoiceCommand::InjectWav`] has been fed in.
    Injected {
        seconds: f32,
    },
    /// The service is gone (ended, or unreachable for [`RECONNECT_WINDOW`]); sent once, last.
    Disconnected,
}

impl VoiceEvent {
    /// One event from its JSON text; `None` for anything that isn't a known event.
    pub fn parse(text: &str) -> Option<VoiceEvent> {
        VoiceEvent::from_value(&serde_json::from_str(text).ok()?)
    }

    /// One event from its JSON value; `None` for anything that isn't a known event.
    pub fn from_value(v: &Value) -> Option<VoiceEvent> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
        let b = |k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
        let f = |k: &str| v.get(k).and_then(Value::as_f64).filter(|x| x.is_finite()).unwrap_or(0.0) as f32;
        let names = |k: &str| -> Vec<String> {
            v.get(k)
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .take(512)
                        .filter_map(|d| d.as_str().map(str::to_string).or_else(|| d.get("name").and_then(Value::as_str).map(str::to_string)))
                        .collect()
                })
                .unwrap_or_default()
        };
        Some(match v.get("type")?.as_str()? {
            "state" => {
                let n = |k: &str| v.get(k).and_then(Value::as_u64).map_or(0, |x| x.min(1000) as u32);
                VoiceEvent::State {
                    listening: b("listening"),
                    capturing: b("capturing"),
                    speaking: b("speaking"),
                    mode: s("mode"),
                    enrolled: b("enrolled"),
                    models_ready: v.get("models_ready").and_then(Value::as_bool).unwrap_or(true),
                    loading: s("loading"),
                    load_step: n("load_step"),
                    load_steps: n("load_steps"),
                }
            }
            "wake" => VoiceEvent::Wake { word: s("word"), score: f("score") },
            "partial" => VoiceEvent::Partial { text: s("text") },
            "utterance" => VoiceEvent::Utterance {
                text: s("text"),
                speaker_ok: b("speaker_ok"),
                speaker_score: f("speaker_score"),
                duration_s: f("duration_s"),
                verified: v.get("verified").and_then(Value::as_bool).unwrap_or(true),
            },
            "barge_in" => VoiceEvent::BargeIn,
            "spoken" => VoiceEvent::Spoken { id: s("id"), interrupted: b("interrupted") },
            "enroll_progress" => VoiceEvent::EnrollProgress { seconds: f("seconds"), needed: f("needed") },
            "enrolled" => VoiceEvent::Enrolled { ok: b("ok") },
            "voice_designed" => VoiceEvent::VoiceDesigned { ref_audio: PathBuf::from(s("ref_audio")), ref_text: s("ref_text") },
            "devices" => VoiceEvent::Devices { inputs: names("inputs"), outputs: names("outputs") },
            "error" => VoiceEvent::Error { message: s("message"), fatal: b("fatal") },
            "config_ok" => VoiceEvent::ConfigOk,
            "level" => VoiceEvent::Level { input: f("in").clamp(0.0, 1.0), output: f("out").clamp(0.0, 1.0) },
            "injected" => VoiceEvent::Injected { seconds: f("seconds") },
            "echo_ignored" => VoiceEvent::EchoIgnored { text: s("text"), reason: s("reason") },
            "status" => {
                let mut state = v.clone();
                if let Some(o) = state.as_object_mut() {
                    o.remove("type");
                }
                VoiceEvent::Status { state }
            }
            _ => return None,
        })
    }
}

/// Tunables (tests shorten them).
#[derive(Debug, Clone, Copy)]
pub(crate) struct ClientOptions {
    pub reconnect_window: Duration,
    pub connect_wait: Duration,
}

impl Default for ClientOptions {
    fn default() -> Self {
        ClientOptions { reconnect_window: RECONNECT_WINDOW, connect_wait: CONNECT_WAIT }
    }
}

/// What the I/O thread is given to send.
enum Out {
    Text(String),
    Close,
}

type SharedProcess = Arc<Mutex<Option<RunningProcess>>>;
type Log = Arc<Mutex<VecDeque<String>>>;

/// What the client and its I/O thread share.
struct Shared {
    subs: Mutex<Subscribers>,
    alive: AtomicBool,
    disconnected: AtomicBool,
    /// The last configuration sent (JSON), sent again after a reconnect.
    last_config: Mutex<Option<String>>,
}

/// Who listens, and what came before anyone did.
#[derive(Default)]
struct Subscribers {
    list: Vec<SyncSender<VoiceEvent>>,
    /// Events that came before the first subscriber (up to [`EARLY_EVENTS`]): it gets them all,
    /// so nothing said right after connecting is lost. `None` once someone has subscribed.
    early: Option<VecDeque<VoiceEvent>>,
    /// The last state, replayed to each later subscriber.
    last_state: Option<VoiceEvent>,
}

impl Shared {
    fn broadcast(&self, ev: VoiceEvent) {
        let mut subs = self.subs.lock().unwrap_or_else(PoisonError::into_inner);
        if matches!(ev, VoiceEvent::State { .. }) {
            subs.last_state = Some(ev.clone());
        }
        if let Some(early) = subs.early.as_mut() {
            if early.len() >= EARLY_EVENTS {
                early.pop_front();
            }
            early.push_back(ev.clone());
        }
        subs.list.retain(|s| match s.try_send(ev.clone()) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                log::warn!("a voice event listener fell {SUBSCRIBER_BACKLOG} events behind; dropping one for it");
                true
            }
            Err(TrySendError::Disconnected(_)) => false,
        });
    }

    /// The link is gone for good: tell everyone, once.
    fn finish(&self) {
        self.alive.store(false, Ordering::SeqCst);
        if !self.disconnected.swap(true, Ordering::SeqCst) {
            self.broadcast(VoiceEvent::Disconnected);
        }
    }
}

/// A running voice service and the WebSocket to it. Dropping it shuts the service down.
pub struct VoiceClient {
    shared: Arc<Shared>,
    tx: Sender<Out>,
    stop: CancelToken,
    io: Mutex<Option<JoinHandle<()>>>,
    process: SharedProcess,
    log: Log,
    port: u16,
    pid: Option<u32>,
    shut: AtomicBool,
}

impl std::fmt::Debug for VoiceClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoiceClient").field("port", &self.port).field("pid", &self.pid).field("alive", &self.shared.alive.load(Ordering::SeqCst)).finish()
    }
}

fn connect_ws(port: u16, wait: Duration, cancel: &CancelToken) -> Result<WsConn, ToolError> {
    let ep = crate::cdp::parse_endpoint(&format!("http://127.0.0.1:{port}"))?;
    let target = crate::cdp::ws_target(&format!("ws://127.0.0.1:{port}/ws"), &ep)?;
    WsConn::connect(&target, MAX_MESSAGE, &Deadline::new(wait, cancel.clone()))
}

/// Connect, trying again until `wait` is over (a service may say READY a moment before it accepts).
fn connect_retrying(port: u16, wait: Duration, cancel: &CancelToken) -> Result<WsConn, ToolError> {
    let t0 = Instant::now();
    loop {
        let left = wait.saturating_sub(t0.elapsed()).max(Duration::from_millis(200));
        match connect_ws(port, left.min(Duration::from_secs(2)), cancel) {
            Ok(c) => return Ok(c),
            Err(e) if e.code == ErrorCode::Cancelled => return Err(e),
            Err(e) if t0.elapsed() >= wait => {
                return Err(ToolError::new(ErrorCode::EngineMissing, format!("Couldn't connect to the voice service on port {port}: {}", e.message))
                    .with_remediation("Check that the voice service is running (voice_status)."));
            }
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

fn push_log(log: &Log, line: &str) {
    let mut l = log.lock().unwrap_or_else(PoisonError::into_inner);
    if l.len() >= LOG_LINES {
        l.pop_front();
    }
    l.push_back(crate::capped_line(line, 2000).to_string());
}

fn log_tail(log: &Log, n: usize) -> Vec<String> {
    let l = log.lock().unwrap_or_else(PoisonError::into_inner);
    l.iter().skip(l.len().saturating_sub(n)).cloned().collect()
}

impl VoiceClient {
    /// Start the voice service (`<python> -I voice_service.py --host 127.0.0.1 --port <port>
    /// --models-dir <dir> --device <dev> --parent-pid <this app>`), wait for its `READY` line (up
    /// to [`VoiceSetup::ready_timeout`]) and connect. Refused when the Python isn't there or the
    /// port is taken; the service is stopped again when anything fails.
    pub fn start(setup: &VoiceSetup) -> Result<VoiceClient, ToolError> {
        Self::start_cancellable(setup, &CancelToken::default())
    }

    /// [`VoiceClient::start`] that stops waiting for `READY` (and stops the service) when `cancel`
    /// is cancelled (added to the contract: tools are cancellable).
    pub fn start_cancellable(setup: &VoiceSetup, cancel: &CancelToken) -> Result<VoiceClient, ToolError> {
        let cmd = setup.command()?;
        match port_state(setup.port) {
            super::PortState::Free => {}
            super::PortState::Serving { .. } => {
                return Err(ToolError::new(ErrorCode::BadSettings, format!("A voice service is already running on port {}.", setup.port))
                    .with_remediation("Stop it first (voice_stop), or set another port in Settings → Voice (voice.port)."));
            }
            super::PortState::Busy { detail } => {
                return Err(ToolError::new(ErrorCode::BadSettings, format!("Port {} is used by another program ({detail}).", setup.port))
                    .with_remediation("Set another port in Settings → Voice (voice.port)."));
            }
        }
        Self::launch(cmd, setup.port, setup.ready_timeout, cancel, ClientOptions::default())
    }

    /// Connect to a voice service that is already running on `127.0.0.1:<port>` (added to the
    /// contract: for a service started elsewhere, and for tests). Shutting this client down asks
    /// that service to end too.
    pub fn connect(port: u16) -> Result<VoiceClient, ToolError> {
        Self::connect_with(port, ClientOptions::default())
    }

    pub(crate) fn connect_with(port: u16, opts: ClientOptions) -> Result<VoiceClient, ToolError> {
        let stop = CancelToken::default();
        let conn = connect_retrying(port, opts.connect_wait, &stop)?;
        Ok(Self::from_parts(conn, port, None, Arc::new(Mutex::new(None)), Arc::new(Mutex::new(VecDeque::new())), stop, opts))
    }

    /// Start `cmd`, wait for `READY`, connect.
    pub(crate) fn launch(cmd: ServiceCommand, port: u16, ready_timeout: Duration, cancel: &CancelToken, opts: ClientOptions) -> Result<VoiceClient, ToolError> {
        let log: Log = Arc::new(Mutex::new(VecDeque::new()));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<String>();
        let (out_log, err_log) = (log.clone(), log.clone());
        let mut p = Process::new(&cmd.program)
            .args(&cmd.args)
            .timeout(SERVICE_LIFETIME)
            .stdout_cap(256 * 1024)
            .stderr_cap(256 * 1024)
            .on_stdout_line(move |l| {
                if l.trim_start().starts_with("READY") {
                    let _ = ready_tx.send(l.trim().to_string());
                } else if !l.trim().is_empty() {
                    push_log(&out_log, l);
                }
            })
            .on_stderr_line(move |l| push_log(&err_log, l))
            .label("the voice service")
            .remediation("Set the Python that has the voice service's packages in Settings → Voice (voice.python).");
        for (k, v) in &cmd.env {
            p = p.env(k, v);
        }
        if let Some(dir) = &cmd.cwd {
            p = p.cwd(dir);
        }
        let mut running = p.spawn()?;
        let pid = running.pid();
        let t0 = Instant::now();
        let ready = loop {
            match ready_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(line) => break line,
                Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => {}
            }
            if running.is_finished() {
                // Its last words may still be on their way to the log.
                std::thread::sleep(Duration::from_millis(100));
                let tail = log_tail(&log, 12).join("\n");
                let why = if tail.trim().is_empty() { "it printed nothing".to_string() } else { tail };
                return Err(ToolError::new(ErrorCode::ProgramFailed, format!("The voice service stopped before it was ready: {why}"))
                    .with_remediation("Check that the voice Python has the voice service's packages (Settings → Voice)."));
            }
            if cancel.is_cancelled() {
                running.kill_and_wait(Duration::from_secs(3));
                return Err(ToolError::cancelled());
            }
            if t0.elapsed() >= ready_timeout {
                running.kill_and_wait(Duration::from_secs(3));
                let tail = log_tail(&log, 12).join("\n");
                return Err(ToolError::new(
                    ErrorCode::TimedOut,
                    format!("The voice service didn't say it was ready within {} s.", ready_timeout.as_secs().max(1)),
                )
                .with_remediation("A first start downloads and loads the speech models and can take long; look at its log and try again.")
                .with_details(json!({"log": tail})));
            }
        };
        let Some((ready_port, version)) = parse_ready(&ready) else {
            running.kill_and_wait(Duration::from_secs(3));
            return Err(ToolError::new(
                ErrorCode::GarbledOutput,
                format!("The voice service's READY line didn't make sense: {}", crate::capped_line(&ready, 300)),
            ));
        };
        if version != PROTOCOL_VERSION {
            running.kill_and_wait(Duration::from_secs(3));
            return Err(ToolError::new(
                ErrorCode::NotSupported,
                format!("The voice service speaks protocol version {version}; this app speaks version {PROTOCOL_VERSION}."),
            )
            .with_remediation("Update Craft Anything, or use the voice service that ships with it."));
        }
        if ready_port != port {
            log::warn!("the voice service listens on port {ready_port}, not the {port} it was given");
        }
        let stop = CancelToken::default();
        let conn = match connect_retrying(ready_port, opts.connect_wait, &stop) {
            Ok(c) => c,
            Err(e) => {
                running.kill_and_wait(Duration::from_secs(3));
                return Err(e);
            }
        };
        Ok(Self::from_parts(conn, ready_port, Some(pid), Arc::new(Mutex::new(Some(running))), log, stop, opts))
    }

    fn from_parts(conn: WsConn, port: u16, pid: Option<u32>, process: SharedProcess, log: Log, stop: CancelToken, opts: ClientOptions) -> VoiceClient {
        let shared = Arc::new(Shared {
            subs: Mutex::new(Subscribers { early: Some(VecDeque::new()), ..Subscribers::default() }),
            alive: AtomicBool::new(true),
            disconnected: AtomicBool::new(false),
            last_config: Mutex::new(None),
        });
        let (tx, rx) = std::sync::mpsc::channel::<Out>();
        let io = {
            let (shared, process, stop) = (shared.clone(), process.clone(), stop.clone());
            std::thread::Builder::new().name("voice-io".into()).spawn(move || io_loop(conn, port, rx, &shared, &process, &stop, opts))
        };
        let io = match io {
            Ok(h) => Some(h),
            Err(e) => {
                log::warn!("couldn't start the voice thread: {e}");
                shared.finish();
                None
            }
        };
        VoiceClient { shared, tx, stop, io: Mutex::new(io), process, log, port, pid, shut: AtomicBool::new(false) }
    }

    /// A new receiver of every event from now on (each call is its own subscriber; any number may
    /// listen). The first subscriber also gets what the service said since connecting (up to 256
    /// events), so nothing is lost between `start` and `events`; a later one gets the last state
    /// first. [`VoiceEvent::Disconnected`] comes at once when the client is already dead.
    pub fn events(&self) -> Receiver<VoiceEvent> {
        let (tx, rx) = std::sync::mpsc::sync_channel(SUBSCRIBER_BACKLOG);
        let mut subs = self.shared.subs.lock().unwrap_or_else(PoisonError::into_inner);
        let mut told_gone = false;
        match subs.early.take() {
            Some(early) => {
                for ev in early {
                    told_gone |= ev == VoiceEvent::Disconnected;
                    let _ = tx.try_send(ev);
                }
            }
            None => {
                if let Some(state) = subs.last_state.clone() {
                    let _ = tx.try_send(state);
                }
            }
        }
        // Checked under the lock: a `finish` that set the flag but hasn't broadcast yet will
        // broadcast to the list, so either it reaches this one there or it's sent here, once.
        if self.shared.disconnected.load(Ordering::SeqCst) {
            if !told_gone {
                let _ = tx.try_send(VoiceEvent::Disconnected);
            }
        } else {
            subs.list.push(tx);
        }
        rx
    }

    /// Send one command (queued; sent within a few milliseconds, or after a reconnect).
    pub fn send(&self, cmd: VoiceCommand) -> Result<(), ToolError> {
        if !self.is_alive() {
            return Err(ToolError::new(ErrorCode::EngineMissing, "The voice service isn't running.").with_remediation("Start it (voice_start)."));
        }
        let text = cmd.to_json()?;
        if matches!(cmd, VoiceCommand::Config(_)) {
            *self.shared.last_config.lock().unwrap_or_else(PoisonError::into_inner) = Some(text.clone());
        }
        self.tx.send(Out::Text(text)).map_err(|_| ToolError::new(ErrorCode::EngineMissing, "The voice service isn't connected any more."))
    }

    /// Speak `text` (interruptible by the person's voice); [`VoiceEvent::Spoken`] with `id` when done.
    pub fn speak(&self, id: &str, text: &str) -> Result<(), ToolError> {
        self.send(VoiceCommand::Speak { id: id.to_string(), text: text.to_string(), interruptible: true })
    }

    pub fn stop_speaking(&self) -> Result<(), ToolError> {
        self.send(VoiceCommand::StopSpeaking)
    }

    /// The push-to-talk key went down (`true`) or up.
    pub fn ptt(&self, down: bool) -> Result<(), ToolError> {
        self.send(VoiceCommand::Ptt { down })
    }

    /// Apply settings (kept, and sent again after a reconnect).
    pub fn config(&self, config: &VoiceConfig) -> Result<(), ToolError> {
        self.send(VoiceCommand::Config(config.clone()))
    }

    pub fn enroll_start(&self) -> Result<(), ToolError> {
        self.send(VoiceCommand::EnrollStart)
    }

    pub fn enroll_stop(&self) -> Result<(), ToolError> {
        self.send(VoiceCommand::EnrollStop)
    }

    pub fn design_voice(&self, description: &str, sample_text: &str) -> Result<(), ToolError> {
        self.send(VoiceCommand::DesignVoice { description: description.to_string(), sample_text: sample_text.to_string() })
    }

    /// Ask for the audio devices ([`VoiceEvent::Devices`]).
    pub fn devices(&self) -> Result<(), ToolError> {
        self.send(VoiceCommand::Devices)
    }

    /// Ask for the state ([`VoiceEvent::Status`]).
    pub fn request_status(&self) -> Result<(), ToolError> {
        self.send(VoiceCommand::Status)
    }

    /// The service runs and the link to it is up (or being restored).
    pub fn is_alive(&self) -> bool {
        if !self.shared.alive.load(Ordering::SeqCst) {
            return false;
        }
        let mut p = self.process.lock().unwrap_or_else(PoisonError::into_inner);
        !p.as_mut().is_some_and(RunningProcess::is_finished)
    }

    /// The port it listens on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The service's process id, when this client started it.
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// The service's last `n` log lines (what it printed besides `READY`).
    pub fn log_tail(&self, n: usize) -> Vec<String> {
        log_tail(&self.log, n)
    }

    /// Ask the service to end, wait for it (a few seconds), then make sure: the link is closed,
    /// the process killed if it's still there, and every subscriber told
    /// [`VoiceEvent::Disconnected`]. Safe to call more than once.
    pub fn shutdown(&self) {
        if self.shut.swap(true, Ordering::SeqCst) {
            return;
        }
        if self.shared.alive.load(Ordering::SeqCst)
            && let Ok(text) = VoiceCommand::Shutdown.to_json()
        {
            let _ = self.tx.send(Out::Text(text));
        }
        let _ = self.tx.send(Out::Close);
        let handle = self.io.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(h) = handle {
            let t0 = Instant::now();
            while !h.is_finished() && t0.elapsed() < Duration::from_secs(2) {
                std::thread::sleep(Duration::from_millis(10));
            }
            self.stop.cancel();
            let t1 = Instant::now();
            while !h.is_finished() && t1.elapsed() < Duration::from_secs(1) {
                std::thread::sleep(Duration::from_millis(10));
            }
            if h.is_finished() {
                let _ = h.join();
            }
        }
        self.stop.cancel();
        let running = self.process.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(mut running) = running
            && running.wait_timeout(SHUTDOWN_WAIT).is_none()
        {
            log::info!("the voice service didn't end when asked; stopping it");
            running.kill_and_wait(Duration::from_secs(3));
        }
        self.shared.finish();
    }
}

impl Drop for VoiceClient {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The I/O thread: send what's queued, read what comes, reconnect when the link drops.
fn io_loop(mut conn: WsConn, port: u16, rx: Receiver<Out>, shared: &Shared, process: &SharedProcess, stop: &CancelToken, opts: ClientOptions) {
    // A command whose send failed when the link dropped, sent first after reconnecting.
    let mut pending: VecDeque<String> = VecDeque::new();
    let short = || Deadline::new(Duration::from_secs(2), stop.clone());
    loop {
        let mut lost = false;
        // What's queued.
        while let Some(text) = pending.pop_front() {
            if conn.send_text(&text, &short()).is_err() {
                pending.push_front(text);
                lost = true;
                break;
            }
        }
        while !lost {
            match rx.try_recv() {
                Ok(Out::Text(text)) => {
                    if conn.send_text(&text, &short()).is_err() {
                        pending.push_back(text);
                        lost = true;
                    }
                }
                Ok(Out::Close) | Err(TryRecvError::Disconnected) => {
                    conn.close(&short());
                    shared.finish();
                    return;
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        if stop.is_cancelled() {
            conn.close(&short());
            shared.finish();
            return;
        }
        // What comes.
        if !lost {
            match conn.read_message(&Deadline::new(READ_SLICE, stop.clone())) {
                Ok(Some(bytes)) => {
                    if let Some(ev) = std::str::from_utf8(&bytes).ok().and_then(VoiceEvent::parse) {
                        shared.broadcast(ev);
                    } else {
                        log::debug!("voice: skipped a message that isn't a known event");
                    }
                }
                Ok(None) => lost = true,
                Err(e) if e.code == ErrorCode::TimedOut => {}
                Err(e) if e.code == ErrorCode::Cancelled => {
                    conn.close(&short());
                    shared.finish();
                    return;
                }
                Err(e) => {
                    log::info!("voice: the link dropped: {}", e.message);
                    lost = true;
                }
            }
        }
        if lost {
            match reconnect(port, shared, process, stop, opts) {
                Some(c) => conn = c,
                None => {
                    // Gone for good: don't leave a service nobody can reach.
                    if let Some(mut running) = process.lock().unwrap_or_else(PoisonError::into_inner).take() {
                        running.kill_and_wait(Duration::from_secs(3));
                    }
                    shared.finish();
                    return;
                }
            }
        }
    }
}

/// Connect again while the service runs, for at most the reconnect window; then send the last
/// configuration and ask for the state.
fn reconnect(port: u16, shared: &Shared, process: &SharedProcess, stop: &CancelToken, opts: ClientOptions) -> Option<WsConn> {
    let t0 = Instant::now();
    loop {
        if stop.is_cancelled() || t0.elapsed() >= opts.reconnect_window {
            return None;
        }
        if process.lock().unwrap_or_else(PoisonError::into_inner).as_mut().is_some_and(RunningProcess::is_finished) {
            log::info!("voice: the service has ended");
            return None;
        }
        let left = opts.reconnect_window.saturating_sub(t0.elapsed()).clamp(Duration::from_millis(100), Duration::from_secs(1));
        match connect_ws(port, left, stop) {
            Ok(mut conn) => {
                log::info!("voice: reconnected to the service on port {port}");
                let dl = Deadline::new(Duration::from_secs(2), stop.clone());
                let config = shared.last_config.lock().unwrap_or_else(PoisonError::into_inner).clone();
                let resent = config.is_none_or(|c| conn.send_text(&c, &dl).is_ok());
                let asked = VoiceCommand::Status.to_json().is_ok_and(|s| conn.send_text(&s, &dl).is_ok());
                if resent && asked {
                    return Some(conn);
                }
            }
            Err(e) if e.code == ErrorCode::Cancelled => return None,
            Err(_) => {}
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}
