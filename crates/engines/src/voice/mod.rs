//! Talking with Craft Anything by voice: the local voice service and the app's side of it.
//!
//! The service is a Python program ([`VOICE_SERVICE_PY`], shipped inside the app and written to
//! [`VoiceSetup::scripts_dir`] when it starts) run with the person's "voice Python": microphone,
//! push-to-talk or wake word, speech to text, a check that it's the person's own voice, and speech
//! out with barge-in. Everything stays on this computer. It's started by [`VoiceClient::start`]
//! as an owned process (it ends with this app: its group is killed when the client is dropped,
//! and it watches `--parent-pid` itself), says `READY {"port":…,"version":1}` once it listens,
//! and then talks JSON over a WebSocket on loopback (`ws://127.0.0.1:<port>/ws`): commands go out
//! ([`VoiceCommand`]), events come back ([`VoiceEvent`]). The protocol is `docs/voice-protocol.md`.
//!
//! [`status`] says, quickly and without starting anything, whether it can run: the Python, the
//! script, the port and the models. [`standin::StandInService`] is an in-process stand-in that
//! speaks the protocol, for tests and demos.

pub mod client;
pub mod standin;

pub use client::{ListenMode, TtsVoice, VoiceClient, VoiceCommand, VoiceConfig, VoiceEvent};

use craft_anything_evidence::{ErrorCode, ToolError};
use craft_anything_platform::cancel::CancelToken;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::ffi::OsString;
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The voice service (Python): `scripts/voice_service.py`, its packages in
/// `scripts/voice_requirements.txt`.
pub const VOICE_SERVICE_PY: &str = include_str!("scripts/voice_service.py");
/// Its file name in [`VoiceSetup::scripts_dir`].
pub const SCRIPT_NAME: &str = "voice_service.py";
/// The port the service listens on unless the settings say otherwise.
pub const DEFAULT_PORT: u16 = 8191;
/// The protocol version this app speaks (the `version` in `READY` and `/health`).
pub const PROTOCOL_VERSION: u64 = 1;
/// How long a started service has to say `READY` (loading the speech models can take a while).
pub const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(3 * 60);
/// The marker the placeholder service carries.
const PLACEHOLDER_MARK: &str = "CRAFT-ANYTHING-VOICE-PLACEHOLDER";

/// Where the voice service and its parts are.
#[derive(Debug, Clone, PartialEq)]
pub struct VoiceSetup {
    /// The Python that has the voice service's packages; none: voice isn't set up.
    pub python: Option<PathBuf>,
    /// The loopback port it listens on ([`DEFAULT_PORT`]).
    pub port: u16,
    /// Where its models are kept (downloaded there on first use).
    pub models_dir: PathBuf,
    /// `cuda:0`, `cpu`…
    pub device: String,
    /// Where the service script is written (added to the contract: the script ships inside the app).
    pub scripts_dir: PathBuf,
    /// How long it has to say `READY` (added to the contract; [`DEFAULT_READY_TIMEOUT`]).
    pub ready_timeout: Duration,
    /// Tests only: start it with `--allow-inject`, so [`VoiceCommand::InjectWav`] can feed a WAV
    /// in place of the microphone. Never set from the settings.
    pub allow_inject: bool,
}

impl Default for VoiceSetup {
    fn default() -> Self {
        VoiceSetup {
            python: None,
            port: DEFAULT_PORT,
            models_dir: PathBuf::new(),
            device: "cuda:0".into(),
            scripts_dir: PathBuf::new(),
            ready_timeout: DEFAULT_READY_TIMEOUT,
            allow_inject: false,
        }
    }
}

impl VoiceSetup {
    /// Where the service script is (or will be) written.
    pub fn script_path(&self) -> PathBuf {
        self.scripts_dir.join(SCRIPT_NAME)
    }

    /// The configured Python, when it's there.
    pub fn python_found(&self) -> Option<&Path> {
        self.python.as_deref().filter(|p| p.is_file())
    }

    /// Write the service script (when missing or changed); its path.
    pub fn write_script(&self) -> Result<PathBuf, ToolError> {
        if self.scripts_dir.as_os_str().is_empty() {
            return Err(ToolError::internal("the voice service has no folder to be written to"));
        }
        std::fs::create_dir_all(&self.scripts_dir).map_err(|e| ToolError::io(self.scripts_dir.display(), e))?;
        let p = self.script_path();
        if std::fs::read_to_string(&p).ok().as_deref() != Some(VOICE_SERVICE_PY) {
            std::fs::write(&p, VOICE_SERVICE_PY).map_err(|e| ToolError::io(p.display(), e))?;
        }
        Ok(p)
    }

    /// The command that starts the service: the Python is checked, the script written and the
    /// models folder made.
    pub(crate) fn command(&self) -> Result<ServiceCommand, ToolError> {
        let python = self.python_found().ok_or_else(|| python_missing(self.python.as_deref()))?.to_path_buf();
        let script = self.write_script()?;
        if !self.models_dir.as_os_str().is_empty() {
            std::fs::create_dir_all(&self.models_dir).map_err(|e| ToolError::io(self.models_dir.display(), e))?;
        }
        let device = if self.device.trim().is_empty() { "cpu".to_string() } else { self.device.trim().to_string() };
        let mut args: Vec<OsString> = vec!["-I".into(), script.into_os_string()];
        args.extend(["--host".into(), "127.0.0.1".into(), "--port".into(), self.port.to_string().into()]);
        args.extend(["--models-dir".into(), self.models_dir.clone().into_os_string(), "--device".into(), device.into()]);
        args.extend(["--parent-pid".into(), std::process::id().to_string().into()]);
        if self.allow_inject {
            args.push("--allow-inject".into());
        }
        Ok(ServiceCommand {
            program: python.into_os_string(),
            args,
            env: vec![("PYTHONUTF8".into(), "1".into()), ("PYTHONIOENCODING".into(), "utf-8".into()), ("PYTHONUNBUFFERED".into(), "1".into())],
            cwd: Some(self.scripts_dir.clone()),
        })
    }
}

fn python_missing(set: Option<&Path>) -> ToolError {
    let what = match set {
        Some(p) => format!("Voice isn't set up: {} isn't there.", p.display()),
        None => "Voice isn't set up: no voice Python is set.".to_string(),
    };
    ToolError::new(ErrorCode::EngineMissing, what).with_remediation("Set the Python that has the voice service's packages in Settings → Voice (voice.python).")
}

/// How to start the service process (the real one, or a test's stand-in).
#[derive(Debug, Clone)]
pub(crate) struct ServiceCommand {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub env: Vec<(OsString, OsString)>,
    pub cwd: Option<PathBuf>,
}

/// The shipped service is still the placeholder (it hears and says nothing).
pub fn service_is_placeholder() -> bool {
    VOICE_SERVICE_PY.contains(PLACEHOLDER_MARK)
}

/// What is on the voice port.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum PortState {
    /// Nothing listens there.
    Free,
    /// A voice service answers there (`GET /health`), with its protocol version and state.
    Serving { version: u64, state: Value },
    /// Something else has it.
    Busy { detail: String },
}

/// Whether voice can run here, part by part.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VoiceStatus {
    /// The voice Python is set and there.
    pub python_found: bool,
    pub python: Option<PathBuf>,
    /// The service script is written (it is written again on every start).
    pub script_present: bool,
    pub script: PathBuf,
    /// The shipped service is the placeholder that hears and says nothing.
    pub placeholder: bool,
    pub port: u16,
    pub port_state: PortState,
    /// The models folder has something in it (they're downloaded there on first use).
    pub models_present: bool,
    pub models_dir: PathBuf,
    /// It can be started now (the Python is there and the port is free).
    pub startable: bool,
    /// A voice service is already answering on the port.
    pub serving: bool,
    /// In words.
    pub detail: String,
}

/// Look at every part without starting anything (one short loopback connection at most).
pub fn status(setup: &VoiceSetup) -> VoiceStatus {
    let python_found = setup.python_found().is_some();
    let script = setup.script_path();
    let script_present = std::fs::read_to_string(&script).ok().as_deref() == Some(VOICE_SERVICE_PY);
    let port_state = port_state(setup.port);
    let models_present = std::fs::read_dir(&setup.models_dir).map(|mut d| d.next().is_some()).unwrap_or(false);
    let serving = matches!(port_state, PortState::Serving { .. });
    let startable = python_found && port_state == PortState::Free;
    let detail = if serving {
        format!("A voice service is running on port {}.", setup.port)
    } else if !python_found {
        python_missing(setup.python.as_deref()).message
    } else if let PortState::Busy { detail } = &port_state {
        format!("Port {} is used by another program ({detail}). Pick another port in Settings → Voice.", setup.port)
    } else if !models_present {
        "Ready to start; its speech models are downloaded on first use.".to_string()
    } else {
        "Ready to start.".to_string()
    };
    VoiceStatus {
        python_found,
        python: setup.python.clone(),
        script_present,
        script,
        placeholder: service_is_placeholder(),
        port: setup.port,
        port_state,
        models_present,
        models_dir: setup.models_dir.clone(),
        startable,
        serving,
        detail,
    }
}

/// What is listening on `127.0.0.1:<port>`: nothing, a voice service, or something else.
pub fn port_state(port: u16) -> PortState {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    match TcpStream::connect_timeout(&addr, Duration::from_millis(400)) {
        Ok(s) => drop(s),
        // Refused, or (Windows retries a refused loopback connect) no answer in time: nobody's there.
        Err(_) => return PortState::Free,
    }
    match health(port) {
        Some((version, state)) => PortState::Serving { version, state },
        None => PortState::Busy { detail: "it doesn't answer as a voice service".into() },
    }
}

/// `GET /health` on the port: the service's protocol version and state, when it answers as one.
pub fn health(port: u16) -> Option<(u64, Value)> {
    let ep = crate::cdp::parse_endpoint(&format!("http://127.0.0.1:{port}")).ok()?;
    let dl = crate::cdp::Deadline::new(Duration::from_millis(1500), CancelToken::default());
    let (code, body) = crate::cdp::http_get(&ep, "/health", &dl).ok()?;
    if code != 200 {
        return None;
    }
    let v: Value = serde_json::from_slice(&body).ok()?;
    if v.get("ok").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let version = v.get("version").and_then(Value::as_u64)?;
    Some((version, v.get("state").cloned().unwrap_or(Value::Null)))
}

/// The `READY {...}` line's port and protocol version.
pub(crate) fn parse_ready(line: &str) -> Option<(u16, u64)> {
    let json = line.trim().strip_prefix("READY")?.trim();
    let v: Value = serde_json::from_str(json).ok()?;
    let port = v.get("port").and_then(Value::as_u64).and_then(|p| u16::try_from(p).ok()).filter(|p| *p != 0)?;
    let version = v.get("version").and_then(Value::as_u64)?;
    Some((port, version))
}

#[cfg(test)]
mod tests;
