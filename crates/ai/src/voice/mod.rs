//! The voice conversation: the person talks, a local model answers and sorts what they say, and
//! Opus (Claude Code with `claude-opus-5-5`) does everything they ask to be done.
//!
//! Standing rules, each enforced in code and proved by a test (`voice::tests`):
//! - **Opus does all instructed work.** An utterance read as an instruction goes to the
//!   [`Executor`] verbatim; the local model is never asked to do it.
//! - **The local model only converses.** [`Converser`] requests are plain messages with no tools
//!   ([`local::request_body`] writes only [`local::BODY_FIELDS`]).
//! - **Only the person's voice.** An utterance whose speaker check failed is shown greyed and
//!   otherwise ignored: no model call, no job.
//! - **Risky actions are confirmed by voice.** Claude Code's permission requests come through
//!   Craft Anything's approval tool into the voice project's inbox, which always asks about
//!   outward actions ([`approvals::ALWAYS_ASKED`]); each is read out and answered by a clear
//!   spoken yes ([`intent::is_affirmative`]) or no, or the on-screen buttons; no answer in two
//!   minutes is a no.
//!
//! The UI drives a [`VoiceBrain`]: commands in ([`BrainCommand`]), events out ([`BrainEvent`]),
//! and a read-only [`JobStatus`].

pub mod approvals;
pub mod brain;
pub mod executor;
pub mod intent;
pub mod local;
#[cfg(test)]
mod tests;

pub use approvals::PromptKind;
pub use brain::{Brain, BrainConfig};
pub use executor::{ExecEvent, Executor, JobControl, JobRequest, OpusExecutor, OpusSetup, VOICE_OPUS_MODEL};
pub use intent::Intent;
pub use local::{ChatMessage, ChatRequest, ChatRole, Converser, LocalModel, Purpose};

use crate::config::AiConfig;
use craft_anything_evidence::ToolError;
use craft_anything_platform::sync::Mutex;
use serde::{Deserialize, Serialize};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, PoisonError};
use std::time::Instant;

/// One finished utterance from the voice service (its `utterance` event).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Utterance {
    pub text: String,
    /// The speaker check passed: it's the enrolled person's voice.
    pub speaker_ok: bool,
    pub speaker_score: f32,
    pub duration_s: f32,
}

impl Utterance {
    /// A verified utterance (tests and typed input).
    pub fn verified(text: impl Into<String>) -> Self {
        Utterance { text: text.into(), speaker_ok: true, speaker_score: 1.0, duration_s: 1.0 }
    }
}

/// What the UI and the voice service tell the brain.
#[derive(Debug, Clone, PartialEq)]
pub enum BrainCommand {
    /// The person said something (only `speaker_ok` ones are acted on).
    Utterance(Utterance),
    /// The on-screen Yes on a waiting approval (or a question's "Yes").
    ApproveYes(String),
    /// The on-screen No.
    ApproveNo(String),
    /// A typed answer to a question Opus asked.
    Answer { id: String, text: String },
    /// Stop the running job (the Stop button).
    Cancel,
    /// Forget the standing Opus session; the next job starts a new one.
    NewConversation,
    /// End the brain: the running job is stopped and what waits is answered no.
    Stop,
}

/// Who a transcript line is from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Speaker {
    /// The person (speaker check passed).
    User,
    /// Someone else's voice, or the check failed: shown greyed, never acted on.
    NotUser,
    /// The assistant's spoken lines.
    Assistant,
    /// Opus's full answer.
    Opus,
    /// Something the app says about itself (the local model is unreachable…).
    Notice,
}

/// One line of the conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptLine {
    pub speaker: Speaker,
    pub text: String,
    /// The speaker check's score, for the person's lines.
    pub score: Option<f32>,
}

/// How a waiting approval or question ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    /// Answered by voice.
    Voice,
    /// Answered with the on-screen buttons or a typed answer.
    Screen,
    /// No answer in time: no.
    TimedOut,
    /// The job was stopped: no.
    Cancelled,
    /// It went away before anyone answered (the job ended).
    Withdrawn,
}

/// What the brain tells the UI and the voice service.
#[derive(Debug, Clone, PartialEq)]
pub enum BrainEvent {
    /// Speak this (the service's `speak` command with this id).
    Say {
        id: String,
        text: String,
    },
    /// A conversation line for the screen.
    Transcript(TranscriptLine),
    JobStarted {
        job: u64,
        instruction: String,
    },
    /// One spoken sentence about a running job.
    JobProgress {
        job: u64,
        text: String,
    },
    /// A job finished: the short spoken summary and Opus's whole answer.
    JobDone {
        job: u64,
        summary: String,
        text: String,
    },
    JobFailed {
        job: u64,
        message: String,
        cancelled: bool,
    },
    /// Something waits for the person: an approval (yes or no) or Opus's question.
    ApprovalNeeded {
        id: String,
        kind: PromptKind,
        text: String,
        detail: String,
        outward: bool,
        choices: Vec<String>,
    },
    ApprovalResolved {
        id: String,
        approved: bool,
        how: Resolution,
    },
    /// The brain has ended.
    Stopped,
}

/// The running job, read-only, for the screen and for status questions.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct JobStatus {
    pub running: Option<RunningJob>,
    /// Instructions waiting for the running job to finish.
    pub queued: Vec<String>,
    /// What waits for the person's answer, as said.
    pub waiting: Vec<String>,
    /// The last finished job's spoken summary.
    pub last_result: Option<String>,
    /// The standing Opus session.
    pub session: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunningJob {
    pub id: u64,
    pub instruction: String,
    pub seconds: u64,
    /// Its latest steps, newest last.
    pub recent: Vec<String>,
}

/// The brain on a thread of its own: send commands, poll events. Nothing here blocks the caller.
pub struct VoiceBrain {
    tx: Sender<BrainCommand>,
    rx: Receiver<BrainEvent>,
    control: JobControl,
    status: Arc<Mutex<JobStatus>>,
}

impl VoiceBrain {
    /// Start a brain talking through `model` and working through `executor`.
    pub fn start(config: BrainConfig, model: Box<dyn Converser>, executor: Box<dyn Executor>) -> Result<VoiceBrain, ToolError> {
        let (tx, commands) = std::sync::mpsc::channel::<BrainCommand>();
        let (out, rx) = std::sync::mpsc::channel();
        let control = executor.control();
        let mut brain = Brain::new(config.clone(), model, executor, out.clone());
        let status = brain.shared_status();
        std::thread::Builder::new()
            .name("voice-brain".into())
            .spawn(move || {
                let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    while !brain.is_stopped() {
                        match commands.recv_timeout(config.tick) {
                            Ok(cmd) => brain.command(cmd),
                            Err(RecvTimeoutError::Timeout) => {}
                            Err(RecvTimeoutError::Disconnected) => brain.command(BrainCommand::Stop),
                        }
                        brain.tick(Instant::now());
                    }
                }));
                if ran.is_err() {
                    let text = "The voice assistant hit a bug and stopped safely.".to_string();
                    let _ = out.send(BrainEvent::Transcript(TranscriptLine { speaker: Speaker::Notice, text, score: None }));
                    let _ = out.send(BrainEvent::Stopped);
                }
            })
            .map_err(|e| ToolError::internal(format!("couldn't start the voice assistant: {e}")))?;
        Ok(VoiceBrain { tx, rx, control, status })
    }

    /// A brain for the person's settings: the Assets role's local model and Claude Code (Opus
    /// 5.5) as set up in `ai`.
    pub fn from_ai_config(ai: &AiConfig, setup: OpusSetup, config: BrainConfig) -> Result<VoiceBrain, ToolError> {
        let model = LocalModel::from_ai_config(ai)?;
        let executor = OpusExecutor::from_ai_config(ai, setup)?;
        VoiceBrain::start(config, Box::new(model), Box::new(executor))
    }

    pub fn send(&self, cmd: BrainCommand) {
        let _ = self.tx.send(cmd);
    }

    /// An utterance from the voice service.
    pub fn utterance(&self, u: Utterance) {
        self.send(BrainCommand::Utterance(u));
    }

    /// Everything the brain said since the last poll.
    pub fn poll(&self) -> Vec<BrainEvent> {
        self.rx.try_iter().collect()
    }

    /// Stop the running job now (not after whatever the brain is in the middle of).
    pub fn cancel(&self) {
        self.control.cancel();
        self.send(BrainCommand::Cancel);
    }

    /// The jobs, as the brain last saw them.
    pub fn status(&self) -> JobStatus {
        self.status.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// End the brain; it says [`BrainEvent::Stopped`].
    pub fn stop(&self) {
        self.send(BrainCommand::Stop);
    }
}

impl Drop for VoiceBrain {
    fn drop(&mut self) {
        let _ = self.tx.send(BrainCommand::Stop);
    }
}
