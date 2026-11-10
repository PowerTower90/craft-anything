//! Opus does the work: every instruction runs as a turn of Claude Code with model
//! `claude-opus-5-5`, through the same runner as the app's builders ([`CliConversation`]): owned
//! process tree, deadlines, stream-json read by the Claude Code adapter, the person's program
//! path and `CLAUDE_CONFIG_DIR` from the backend's options. One standing session per voice
//! conversation: each follow-up resumes it. Permission requests go through Craft Anything's
//! approval tool into the voice project's inbox ([`super::approvals`]).

use super::approvals;
use crate::cli_agents::AgentAdapters;
use crate::cli_agents::{Builder, CliConversation, CliSpec};
use crate::config::{AiConfig, BackendEntry};
use crate::{AgentEvent, BackendConfig, BackendKind, BackendOptions, Conversation};
use craft_anything_engine::CancelToken;
use craft_anything_engine::evidence::{ErrorCode, ToolError};
use craft_anything_engine::inbox::Inbox;
use craft_anything_platform::sync::Mutex;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, PoisonError};

/// The model every voice job runs on.
pub const VOICE_OPUS_MODEL: &str = "claude-opus-5-5";

/// What an instruction is handed to Opus with.
#[derive(Debug, Clone, PartialEq)]
pub struct JobRequest {
    /// The person's words as transcribed, unchanged.
    pub verbatim: String,
    /// The local model's one-sentence reading of them.
    pub instruction: String,
    /// The last lines of the conversation ("User: …", "Assistant: …").
    pub context: Vec<String>,
}

/// What a running job did.
#[derive(Debug, Clone, PartialEq)]
pub enum ExecEvent {
    /// A step: a tool started or ended, a status line. `milestone`: worth saying now.
    Activity { job: u64, text: String, milestone: bool },
    /// Answer text as it streams.
    Text { job: u64, text: String },
    /// The turn ended: its whole answer, the last error if it failed, and whether it was stopped.
    Finished { job: u64, text: String, error: Option<String>, cancelled: bool },
}

/// Stops the running job from any thread, at once.
#[derive(Clone)]
pub struct JobControl {
    requested: Arc<AtomicBool>,
    stop: Arc<dyn Fn() + Send + Sync>,
}

impl JobControl {
    /// A control whose `cancel` calls `stop`.
    pub fn new(stop: impl Fn() + Send + Sync + 'static) -> Self {
        JobControl { requested: Arc::default(), stop: Arc::new(stop) }
    }

    /// Stop the running job: its program and everything it started are ended.
    pub fn cancel(&self) {
        self.requested.store(true, Ordering::SeqCst);
        (self.stop)();
    }

    /// Whether a stop was asked for since the job started.
    pub fn cancel_requested(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }

    fn reset(&self) {
        self.requested.store(false, Ordering::SeqCst);
    }
}

impl std::fmt::Debug for JobControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobControl").field("requested", &self.cancel_requested()).finish()
    }
}

/// What does the instructed work. [`OpusExecutor`] is the real one; the brain's tests use
/// stand-ins.
pub trait Executor: Send {
    /// Start a job; refused while one runs.
    fn submit(&mut self, job: &JobRequest) -> Result<u64, ToolError>;
    /// What happened since the last poll.
    fn poll(&mut self) -> Vec<ExecEvent>;
    /// The handle that stops the running job.
    fn control(&self) -> JobControl;
    fn is_busy(&self) -> bool;
    /// Where the running job's approvals and questions wait.
    fn inbox(&self) -> Option<Inbox>;
    /// The standing session, once Opus has named it (keep it to resume after a restart).
    fn session_id(&self) -> Option<String>;
    /// Forget the standing session: the next job starts a new one.
    fn new_conversation(&mut self);
}

/// Where and how voice jobs run.
#[derive(Debug, Clone, Default)]
pub struct OpusSetup {
    /// The folder Opus works in; `None` or empty: the person's home folder.
    pub task_folder: Option<PathBuf>,
    /// The voice's own folder: its approvals project (`tasks/`) and failure logs (`logs/`).
    /// Keep it out of the projects folder (`<config dir>/voice` is a good place).
    pub state_dir: PathBuf,
    /// Deep mode: our tool server lists every tool.
    pub deep: bool,
    /// A standing session to carry on.
    pub resume: Option<String>,
}

/// Claude Code running Opus 5.5 for voice jobs.
pub struct OpusExecutor {
    spec: CliSpec,
    conversation: Arc<Mutex<CliConversation>>,
    inbox: Inbox,
    job: Option<Running>,
    next_id: u64,
    control: JobControl,
    /// Whether our tool server's helper starts (checked once, in the background).
    tools: Arc<Mutex<Option<Result<(), ToolError>>>>,
}

struct Running {
    id: u64,
    text: String,
    error: Option<String>,
    tools_seen: usize,
}

/// The Claude Code backend voice jobs use: an enabled agent-CLI backend for Claude Code that runs
/// on this computer, the one named "Claude Code" first.
pub fn claude_code_backend(ai: &AiConfig) -> Result<&BackendEntry, ToolError> {
    let claude = |e: &&BackendEntry| {
        e.config.enabled
            && e.config.kind == BackendKind::AgentCli
            && matches!(e.config.model.as_str(), "claude-code" | "claude")
            && e.config.ssh_host.as_deref().is_none_or(|h| h.trim().is_empty())
    };
    ai.backends.iter().filter(claude).find(|e| e.config.name == "Claude Code").or_else(|| ai.backends.iter().find(claude)).ok_or_else(|| {
        ToolError::new(ErrorCode::EngineMissing, "Voice tasks are done by Claude Code, and no Claude Code on this computer is set up.")
            .with_remediation("Add Claude Code in Settings → AI (and sign in to it once in a terminal), then try again.")
    })
}

impl OpusExecutor {
    /// The executor for the Claude Code backend in `ai` ([`claude_code_backend`]).
    pub fn from_ai_config(ai: &AiConfig, setup: OpusSetup) -> Result<Self, ToolError> {
        let entry = claude_code_backend(ai)?;
        OpusExecutor::new(entry.config.clone(), entry.options.clone(), setup, AgentAdapters::built_in())
    }

    /// The executor for `config` (an agent-CLI backend) with its `options`: the program and
    /// Claude Code folder chosen there are kept; the model is always [`VOICE_OPUS_MODEL`].
    pub fn new(config: BackendConfig, mut options: BackendOptions, setup: OpusSetup, adapters: AgentAdapters) -> Result<Self, ToolError> {
        if config.ssh_host.as_deref().is_some_and(|h| !h.trim().is_empty()) {
            return Err(ToolError::new(ErrorCode::BadSettings, format!("{} runs on another computer, and voice tasks run on this one.", config.name))
                .with_remediation("Set up Claude Code on this computer in Settings → AI."));
        }
        let folder = task_folder(setup.task_folder.as_deref())?;
        let ws = approvals::prepare_workspace(&setup.state_dir.join("tasks"))?;
        approvals::clear_inbox(&ws.inbox_dir());
        options.cli_model = Some(VOICE_OPUS_MODEL.to_string());
        // The permission-prompt route needs the plain CLI runner, never ACP.
        options.acp = Some(false);
        let mut spec = CliSpec::new(config, options, false, Some(folder.clone()));
        spec.builder = Some(Builder { investigation: None, inbox: ws.inbox_dir() });
        spec.resume = setup.resume.filter(|r| !r.trim().is_empty());
        spec.deep = setup.deep;
        spec.preamble = Some(preamble(&folder));
        spec.logs = Some(setup.state_dir.join("logs"));
        spec.adapters = adapters;
        let conversation = Arc::new(Mutex::new(CliConversation::new(spec.clone())));
        let stopper = conversation.clone();
        let control = JobControl::new(move || stopper.lock().unwrap_or_else(PoisonError::into_inner).stop());
        let tools: Arc<Mutex<Option<Result<(), ToolError>>>> = Arc::default();
        let (adapter, slot) = (spec.adapters.for_backend(&spec.config.model, &spec.options), tools.clone());
        let _ = std::thread::Builder::new().name("voice-tools-check".into()).spawn(move || {
            let checked = adapter.check_tool_server(&CancelToken::default());
            *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(checked);
        });
        Ok(OpusExecutor { spec, conversation, inbox: Inbox::new(ws.inbox_dir()), job: None, next_id: 0, control, tools })
    }

    /// How the jobs are run (program, folder, model, approvals inbox).
    pub fn spec(&self) -> &CliSpec {
        &self.spec
    }
}

/// The folder a voice job works in: the chosen one, else the home folder.
fn task_folder(chosen: Option<&Path>) -> Result<PathBuf, ToolError> {
    let folder = match chosen.filter(|p| !p.as_os_str().is_empty()) {
        Some(p) => p.to_path_buf(),
        None => dirs::home_dir().ok_or_else(|| {
            ToolError::new(ErrorCode::BadSettings, "There's no home folder for voice tasks to work in.")
                .with_remediation("Choose a task folder in Settings → Voice.")
        })?,
    };
    if !folder.is_dir() {
        return Err(ToolError::new(ErrorCode::NotFound, format!("The voice task folder {} isn't there.", folder.display()))
            .with_remediation("Choose another task folder in Settings → Voice."));
    }
    Ok(folder)
}

/// What Opus is told about voice jobs (appended to Claude Code's system prompt).
pub fn preamble(folder: &Path) -> String {
    format!(
        "You are working for the person through Craft Anything's voice assistant. They asked for this out loud; a small local model passes their words on and reads your progress and answer back to them. It only talks, so do the whole task yourself, on this computer.\n\
         - Your working folder is {}. Use absolute paths for the person's files.\n\
         - Their words came through speech recognition, so names and numbers may be misheard: when one doesn't match anything, ask with core_person_ask rather than guess.\n\
         - Each permission request is read out to them and answered by voice: keep each one to a single clear action.\n\
         - Before anything outward (posting or publishing, sending a message or email, buying or paying, uploading, deleting anything outside a temp folder) first call core_person_ask with the exact text that will be posted or sent and the file or link it uses, and go ahead only on a clear yes.\n\
         - End with a short, plain answer whose first sentence says what you did or found.",
        folder.display()
    )
}

/// The message Opus gets: the person's words unchanged, then what the voice understood and the
/// recent conversation, marked as context.
pub fn compose(job: &JobRequest) -> String {
    let mut text = job.verbatim.trim().to_string();
    let reading = job.instruction.trim();
    let mut notes = vec!["(Spoken instruction, transcribed by speech recognition.".to_string()];
    if !reading.is_empty() && reading != text {
        notes.push(format!("The voice assistant understood it as: \"{reading}\"."));
    }
    if !job.context.is_empty() {
        notes.push(format!("Recent conversation:\n{}", job.context.join("\n")));
    }
    text.push_str("\n\n");
    text.push_str(&notes.join(" "));
    text.push(')');
    text
}

/// A tool call in a few words: its name and its main argument.
fn tool_line(tool: &str, input: &Value) -> String {
    let main = ["command", "file_path", "path", "pattern", "url", "query", "description"]
        .iter()
        .find_map(|k| input.get(*k).and_then(Value::as_str))
        .map(|s| s.chars().take(160).collect::<String>());
    match main {
        Some(m) => format!("{tool}: {m}"),
        None => tool.to_string(),
    }
}

impl Executor for OpusExecutor {
    fn submit(&mut self, job: &JobRequest) -> Result<u64, ToolError> {
        let mut conversation = self.conversation.lock().unwrap_or_else(PoisonError::into_inner);
        if self.job.is_some() || conversation.is_busy() {
            return Err(
                ToolError::new(ErrorCode::OverLimit, "Opus is still working on the last job.").with_remediation("Wait for it to finish, or say cancel.")
            );
        }
        if let Some(Err(e)) = self.tools.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
            return Err(e.clone());
        }
        self.control.reset();
        self.next_id = self.next_id.saturating_add(1);
        let id = self.next_id;
        conversation.send(&compose(job));
        self.job = Some(Running { id, text: String::new(), error: None, tools_seen: 0 });
        Ok(id)
    }

    fn poll(&mut self) -> Vec<ExecEvent> {
        let events = self.conversation.lock().unwrap_or_else(PoisonError::into_inner).poll();
        let mut out = Vec::new();
        for e in events {
            let Some(job) = self.job.as_mut() else { continue };
            let id = job.id;
            match e {
                AgentEvent::TextDelta { text } => {
                    job.text.push_str(&text);
                    out.push(ExecEvent::Text { job: id, text });
                }
                AgentEvent::Status { text } => out.push(ExecEvent::Activity { job: id, text, milestone: false }),
                AgentEvent::ToolStarted { tool, input, .. } => {
                    job.tools_seen = job.tools_seen.saturating_add(1);
                    // The first step is worth saying: it's when the work really starts.
                    out.push(ExecEvent::Activity { job: id, text: tool_line(&tool, &input), milestone: job.tools_seen == 1 });
                }
                AgentEvent::ToolFinished { tool, ok, summary, .. } => {
                    let text = if ok { format!("{tool} done") } else { format!("{tool} failed: {summary}") };
                    out.push(ExecEvent::Activity { job: id, text, milestone: false });
                }
                AgentEvent::Error { message, remediation, .. } => {
                    let text = match remediation {
                        Some(r) => format!("{message} {r}"),
                        None => message,
                    };
                    job.error = Some(text.clone());
                    out.push(ExecEvent::Activity { job: id, text, milestone: true });
                }
                AgentEvent::TurnDone { .. } => {
                    if let Some(done) = self.job.take() {
                        let cancelled = self.control.cancel_requested();
                        out.push(ExecEvent::Finished { job: done.id, text: done.text, error: done.error, cancelled });
                    }
                }
                AgentEvent::Output { text } => log::debug!("voice job {id}: {text}"),
                AgentEvent::ConsentNeeded { .. } => {}
            }
        }
        out
    }

    fn control(&self) -> JobControl {
        self.control.clone()
    }

    fn is_busy(&self) -> bool {
        self.job.is_some()
    }

    fn inbox(&self) -> Option<Inbox> {
        Some(self.inbox.clone())
    }

    fn session_id(&self) -> Option<String> {
        self.conversation.lock().unwrap_or_else(PoisonError::into_inner).session_id()
    }

    fn new_conversation(&mut self) {
        if self.job.is_some() {
            self.control.cancel();
        }
        self.job = None;
        let mut spec = self.spec.clone();
        spec.resume = None;
        *self.conversation.lock().unwrap_or_else(PoisonError::into_inner) = CliConversation::new(spec);
    }
}
