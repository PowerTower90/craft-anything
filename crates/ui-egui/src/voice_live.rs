//! The real voice parts behind the screens ([`LiveVoice`]): the voice service through the
//! engine's `voice_*` tools and its running client (`VoiceSlot`), and the conversation brain with
//! the Opus executor, joined by [`craft_anything_ai::voice::VoiceLink`]. This only turns their
//! events into the picture the screens draw and the person's actions into calls; the rules (only
//! the person's voice, Opus does the work, enrol first, risky actions asked) live in the brain and
//! the engine.
//!
//! Nothing here blocks the UI thread: starting, stopping, checking and saving settings run on
//! threads of their own and report back through a channel read in [`VoiceBackend::view`].

use crate::voice_ui::{
    AssistantVoice, Enrolment, JobState, ListenMode, OpusJob, PartState, PartStatus, Speaker, VoiceAction, VoiceApproval, VoiceBackend, VoiceConfig,
    VoiceDesign, VoiceLine, VoiceParts, VoiceView,
};
use craft_anything_ai::voice::{BrainCommand, BrainConfig, BrainEvent, LinkEvent, OpusSetup, VoiceBrain, VoiceLink};
use craft_anything_engine::tools::voice_tools;
use craft_anything_engine::{CallOptions, Caller, Engine};
use craft_anything_engines::voice::{VoiceClient, VoiceEvent};
use serde_json::{Value, json};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

/// Lines of the conversation kept.
const KEEP_LINES: usize = 300;
/// How long an approval waits before it's a no (the brain's own limit).
const APPROVAL_WAIT: Duration = Duration::from_secs(120);

/// What a background call reported.
enum Done {
    /// The service started (its client), and the brain, or why there's none.
    Started(Result<(Arc<VoiceClient>, Result<VoiceBrain, String>), String>),
    Stopped,
    /// `voice_status`'s answer.
    Status(Result<Value, String>),
    /// Saving the settings failed.
    ConfigFailed(String),
}

/// The real voice parts. See the module docs.
pub struct LiveVoice {
    engine: Arc<Engine>,
    view: VoiceView,
    link: Option<VoiceLink>,
    tx: Sender<Done>,
    rx: Receiver<Done>,
    wake: Option<Arc<dyn Fn() + Send + Sync>>,
    /// The parts were looked at once: when the screens first draw (they set the waker), not when
    /// made, as a look holds the session for a moment and an app being built or closed mustn't.
    checked: bool,
}

impl LiveVoice {
    /// The voice parts for `engine`'s session, with its `[voice]` settings.
    pub fn new(engine: Arc<Engine>) -> LiveVoice {
        let (tx, rx) = mpsc::channel();
        let config = engine.try_session().map(|s| config_of(&s.settings.voice)).unwrap_or_default();
        let view = VoiceView { config, parts: VoiceParts::default(), ..Default::default() };
        LiveVoice { engine, view, link: None, tx, rx, wake: None, checked: false }
    }

    fn call_in_background(&self, what: &'static str, f: impl FnOnce(&Engine) -> Done + Send + 'static) {
        let (engine, tx, wake) = (self.engine.clone(), self.tx.clone(), self.wake.clone());
        let _ = std::thread::Builder::new().name(format!("voice-{what}")).spawn(move || {
            let _ = tx.send(f(&engine));
            if let Some(w) = wake {
                w();
            }
        });
    }

    fn client(&self) -> Option<Arc<VoiceClient>> {
        self.link.as_ref().map(|l| l.client().clone()).filter(|c| c.is_alive())
    }

    /// Send something to the service, or say why it can't be.
    fn send_to_service(&mut self, f: impl FnOnce(&VoiceClient) -> Result<(), craft_anything_engine::evidence::ToolError>) {
        match self.client() {
            Some(c) => {
                if let Err(e) = f(&c) {
                    self.view.error = Some(e.for_people());
                }
            }
            None => self.view.error = Some(crate::i18n::tr("Voice isn't running. Start it with the microphone in the title bar.")),
        }
    }

    fn send_to_brain(&mut self, cmd: BrainCommand) {
        if let Some(l) = &self.link {
            l.brain(|b| b.send(cmd));
        }
    }

    fn start(&mut self) {
        if self.view.running || self.view.starting {
            return;
        }
        self.view.starting = true;
        self.view.error = None;
        self.call_in_background("start", |engine| Done::Started(start_parts(engine)));
    }

    fn check_status(&self) {
        self.call_in_background("status", |engine| Done::Status(call(engine, "voice_status", json!({}))));
    }

    fn save_config(&mut self, config: VoiceConfig) {
        let params = config_params(&config);
        self.view.config = config;
        self.call_in_background("config", move |engine| match call(engine, "voice_config", params) {
            Ok(_) => Done::Status(call(engine, "voice_status", json!({}))),
            Err(e) => Done::ConfigFailed(e),
        });
    }

    fn finished(&mut self, done: Done) {
        match done {
            Done::Started(Ok((client, brain))) => {
                self.view.starting = false;
                self.view.running = client.is_alive();
                match brain {
                    Ok(brain) => {
                        let link = VoiceLink::start(client.clone(), brain);
                        if let Some(w) = &self.wake {
                            link.set_waker(w.clone());
                        }
                        self.link = Some(link);
                        self.view.parts.opus = PartStatus::new(PartState::Ready, crate::i18n::tr("Claude Code with Opus 5.5 does what you ask"));
                    }
                    Err(why) => {
                        self.view.parts.opus = PartStatus::new(PartState::NotSetUp, why.clone());
                        self.view.error = Some(why);
                    }
                }
                let _ = client.devices();
            }
            Done::Started(Err(why)) => {
                self.view.starting = false;
                self.view.running = false;
                self.view.error = Some(why);
            }
            Done::Stopped => {
                self.view.running = false;
                self.view.listening = false;
                self.view.capturing = false;
                self.view.speaking = false;
            }
            Done::Status(Ok(v)) => self.view.parts = parts_of(&v, &self.view),
            Done::Status(Err(why)) | Done::ConfigFailed(why) => self.view.error = Some(why),
        }
    }

    fn fold(&mut self, ev: LinkEvent) {
        match ev {
            LinkEvent::Voice(e) => self.fold_voice(e),
            LinkEvent::Brain(e) => self.fold_brain(e),
        }
    }

    fn fold_voice(&mut self, e: VoiceEvent) {
        let v = &mut self.view;
        match e {
            VoiceEvent::State { listening, capturing, speaking, enrolled, models_ready, loading, load_step, load_steps, .. } => {
                v.ready = models_ready;
                v.loading = (!models_ready).then_some((load_step, load_steps, loading));
                (v.listening, v.capturing, v.speaking, v.enrolled) = (listening, capturing, speaking, enrolled);
                v.running = true;
                v.parts.speaker_profile = if enrolled {
                    PartStatus::new(PartState::Ready, crate::i18n::tr("Your voice is enrolled"))
                } else {
                    PartStatus::new(PartState::NotSetUp, crate::i18n::tr("Not enrolled yet: nothing you say goes to Opus until you are"))
                };
            }
            VoiceEvent::Partial { text } => v.partial = text,
            VoiceEvent::Utterance { .. } => {
                // The local model decides what it was; the brain's next words end the thinking.
                v.partial.clear();
                v.thinking = true;
                v.level_in = 0.0;
            }
            VoiceEvent::Level { input, output } => (v.level_in, v.level_out) = (input, output),
            VoiceEvent::EchoIgnored { text, .. } => {
                // Its own speech came back and was dropped by the service: shown faintly.
                v.thinking = false;
                v.partial.clear();
                v.lines.push(VoiceLine { speaker: Speaker::Echo, text, speaker_score: None });
            }
            VoiceEvent::EnrollProgress { seconds, needed } => v.enrolment = Enrolment::Recording { seconds, needed },
            VoiceEvent::Enrolled { ok } => {
                v.enrolment = Enrolment::Finished { ok };
                v.enrolled |= ok;
            }
            VoiceEvent::VoiceDesigned { ref_audio, ref_text } => v.design = VoiceDesign::Ready(AssistantVoice { ref_audio, ref_text }),
            VoiceEvent::Devices { inputs, outputs } => (v.inputs, v.outputs) = (inputs, outputs),
            VoiceEvent::Error { message, fatal } => {
                if v.design == VoiceDesign::Designing && message.to_ascii_lowercase().contains("design") {
                    v.design = VoiceDesign::Failed(message.clone());
                }
                v.error = Some(message);
                if fatal {
                    v.running = false;
                }
            }
            VoiceEvent::Disconnected => {
                (v.running, v.listening, v.capturing, v.speaking, v.ready, v.thinking) = (false, false, false, false, false, false);
                (v.level_in, v.level_out) = (0.0, 0.0);
                v.partial.clear();
            }
            _ => {}
        }
    }

    fn fold_brain(&mut self, e: BrainEvent) {
        // Anything the brain says, does or shows ends its thinking about what was heard.
        if matches!(
            e,
            BrainEvent::Say { .. }
                | BrainEvent::JobStarted { .. }
                | BrainEvent::ApprovalResolved { .. }
                | BrainEvent::EnrolmentNeeded
                | BrainEvent::Transcript(craft_anything_ai::voice::TranscriptLine { speaker: craft_anything_ai::voice::Speaker::NotUser, .. })
        ) {
            self.view.thinking = false;
        }
        // A short chime when a job is done or something waits for an answer.
        let chime = match &e {
            BrainEvent::JobDone { .. } => Some("done"),
            BrainEvent::ApprovalNeeded { .. } => Some("ask"),
            _ => None,
        };
        if let (Some(kind), true) = (chime, self.view.config.sounds)
            && let Some(c) = self.client()
        {
            let _ = c.send(craft_anything_engines::voice::VoiceCommand::Chime { kind: kind.into() });
        }
        let v = &mut self.view;
        match e {
            BrainEvent::Transcript(line) => {
                let speaker = match line.speaker {
                    craft_anything_ai::voice::Speaker::User => Speaker::You,
                    craft_anything_ai::voice::Speaker::NotUser => Speaker::NotYou,
                    craft_anything_ai::voice::Speaker::Echo => Speaker::Echo,
                    _ => Speaker::Assistant,
                };
                v.lines.push(VoiceLine { speaker, text: line.text, speaker_score: line.score });
                let extra = v.lines.len().saturating_sub(KEEP_LINES);
                v.lines.drain(..extra);
            }
            BrainEvent::JobStarted { job, instruction } => v.jobs.push(OpusJob {
                id: job_id(job),
                task: instruction,
                progress: None,
                latest: String::new(),
                started: Instant::now(),
                state: JobState::Running,
            }),
            BrainEvent::JobProgress { job, text } => {
                if let Some(j) = v.jobs.iter_mut().find(|j| j.id == job_id(job)) {
                    j.latest = text;
                }
            }
            BrainEvent::JobDone { job, summary, .. } => {
                if let Some(j) = v.jobs.iter_mut().find(|j| j.id == job_id(job)) {
                    (j.state, j.latest, j.progress) = (JobState::Done, summary, Some(1.0));
                }
            }
            BrainEvent::JobFailed { job, message, cancelled } => {
                if let Some(j) = v.jobs.iter_mut().find(|j| j.id == job_id(job)) {
                    (j.state, j.latest) = (if cancelled { JobState::Stopped } else { JobState::Failed }, message);
                }
            }
            BrainEvent::ApprovalNeeded { id, text, detail, outward, .. } => {
                v.approvals.retain(|a| a.id != id);
                v.approvals.push(VoiceApproval {
                    id,
                    question: text,
                    content: outward.then_some(detail),
                    public: outward,
                    deadline: Some(Instant::now() + APPROVAL_WAIT),
                });
            }
            BrainEvent::ApprovalResolved { id, .. } => v.approvals.retain(|a| a.id != id),
            BrainEvent::EnrolmentNeeded => v.enrol_asked = v.enrol_asked.wrapping_add(1),
            BrainEvent::Stopped => {
                for j in v.jobs.iter_mut().filter(|j| j.state == JobState::Running) {
                    j.state = JobState::Stopped;
                }
                v.approvals.clear();
            }
            // Spoken by the service already; the transcript line shows it.
            BrainEvent::Say { .. } => {}
        }
    }
}

impl VoiceBackend for LiveVoice {
    fn view(&mut self) -> VoiceView {
        while let Ok(done) = self.rx.try_recv() {
            self.finished(done);
        }
        let events = self.link.as_ref().map(|l| l.poll()).unwrap_or_default();
        for ev in events {
            self.fold(ev);
        }
        self.view.clone()
    }

    fn act(&mut self, action: VoiceAction) {
        match action {
            VoiceAction::Start => self.start(),
            VoiceAction::Stop => {
                // The brain stays, so a running Opus job goes on; only listening and speaking stop.
                self.call_in_background("stop", |engine| {
                    let _ = call(engine, "voice_stop", json!({}));
                    Done::Stopped
                });
            }
            VoiceAction::PushToTalk { down } => self.send_to_service(|c| c.ptt(down)),
            VoiceAction::StopSpeaking => self.send_to_service(VoiceClient::stop_speaking),
            VoiceAction::Answer { id, yes } => {
                self.view.approvals.retain(|a| a.id != id);
                self.send_to_brain(if yes { BrainCommand::ApproveYes(id) } else { BrainCommand::ApproveNo(id) });
            }
            VoiceAction::CancelJob { .. } => {
                if let Some(l) = &self.link {
                    l.brain(|b| b.cancel());
                }
            }
            VoiceAction::EnrolStart => {
                self.view.enrolment = Enrolment::Recording { seconds: 0.0, needed: 20.0 };
                self.send_to_service(VoiceClient::enroll_start);
            }
            VoiceAction::EnrolStop => self.send_to_service(VoiceClient::enroll_stop),
            VoiceAction::DesignVoice { description, sample_text } => {
                self.view.design = VoiceDesign::Designing;
                self.send_to_service(|c| c.design_voice(&description, &sample_text));
            }
            VoiceAction::UseDesignedVoice => {
                if let VoiceDesign::Ready(voice) = self.view.design.clone() {
                    let mut c = self.view.config.clone();
                    c.assistant_voice = Some(voice);
                    self.save_config(c);
                    self.view.design = VoiceDesign::Idle;
                }
            }
            VoiceAction::SetConfig(c) => self.save_config(c),
            VoiceAction::RefreshDevices => self.send_to_service(VoiceClient::devices),
            VoiceAction::CheckStatus => self.check_status(),
        }
        if let Some(w) = &self.wake {
            w();
        }
    }

    fn set_waker(&mut self, wake: Arc<dyn Fn() + Send + Sync>) {
        if !self.checked {
            self.checked = true;
            self.check_status();
        }
        if let Some(l) = &self.link {
            l.set_waker(wake.clone());
        }
        self.wake = Some(wake);
    }
}

fn job_id(job: u64) -> String {
    format!("job-{job}")
}

/// Call a tool as the app and get its value, or the error in the person's words.
fn call(engine: &Engine, tool: &str, params: Value) -> Result<Value, String> {
    engine.call(tool, params, CallOptions::new(Caller::Ui)).map(|o| o.to_json()["result"].clone()).map_err(|e| e.for_people())
}

/// Start the service (the engine's `voice_start`), then the brain on its client: the Assets role's
/// local model to talk, Claude Code with Opus 5.5 to work, in the voice task folder.
fn start_parts(engine: &Engine) -> Result<(Arc<VoiceClient>, Result<VoiceBrain, String>), String> {
    call(engine, "voice_start", json!({}))?;
    let (client, ai, setup) = {
        let s = engine.session();
        let client = voice_tools::running(&s).ok_or_else(|| crate::i18n::tr("The voice service stopped as soon as it started."))?;
        let ai = craft_anything_ai::AiConfig::from_settings(&s.settings);
        let setup = OpusSetup {
            task_folder: s.settings.voice.task_folder.clone(),
            state_dir: s.config_dir().join(voice_tools::VOICE_DIR),
            deep: !s.clean_room(),
            resume: None,
        };
        (client, ai, setup)
    };
    let brain = VoiceBrain::from_ai_config(&ai, setup, BrainConfig::default()).map_err(|e| e.for_people());
    Ok((client, brain))
}

/// The screens' settings from `[voice]`.
fn config_of(v: &craft_anything_engine::settings::VoiceSettings) -> VoiceConfig {
    VoiceConfig {
        listen_mode: ListenMode::from_id(v.listen_mode.id()).unwrap_or_default(),
        wake_word: v.wake_word.clone(),
        push_to_talk_key: v.push_to_talk_key.clone(),
        input_device: v.input_device.clone(),
        output_device: v.output_device.clone(),
        speaker_threshold: v.speaker_threshold,
        task_folder: v.task_folder.clone(),
        assistant_voice: v.assistant_voice_ref_audio.clone().map(|ref_audio| AssistantVoice { ref_audio, ref_text: v.assistant_voice_ref_text.clone() }),
        auto_start: v.auto_start,
        sounds: v.sounds,
        echo_cancellation: v.echo_cancellation,
    }
}

/// `voice_config`'s parameters for the screens' settings (an empty value clears one).
fn config_params(c: &VoiceConfig) -> Value {
    let path = |p: &Option<std::path::PathBuf>| p.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
    json!({
        "listen_mode": c.listen_mode.id(),
        "wake_word": c.wake_word,
        "push_to_talk_key": c.push_to_talk_key,
        "input_device": c.input_device.clone().unwrap_or_default(),
        "output_device": c.output_device.clone().unwrap_or_default(),
        "speaker_threshold": c.speaker_threshold,
        "task_folder": path(&c.task_folder),
        "assistant_voice_ref_audio": c.assistant_voice.as_ref().map(|a| a.ref_audio.display().to_string()).unwrap_or_default(),
        "assistant_voice_ref_text": c.assistant_voice.as_ref().map(|a| a.ref_text.clone()).unwrap_or_default(),
        "auto_start": c.auto_start,
        "sounds": c.sounds,
        "echo_cancellation": c.echo_cancellation,
    })
}

/// Settings → Voice's part lines from `voice_status` and what the service last said.
fn parts_of(status: &Value, now: &VoiceView) -> VoiceParts {
    let service = &status["service"];
    let running = status["running"].as_bool() == Some(true);
    let detail = service["detail"].as_str().unwrap_or_default().to_string();
    let service_part = if running {
        PartStatus::new(PartState::Ready, crate::i18n::tr("Running on this computer"))
    } else if service["python_found"].as_bool() == Some(true) {
        PartStatus::new(PartState::NotRunning, detail.clone())
    } else {
        PartStatus::new(PartState::NotSetUp, detail.clone())
    };
    let models = |ready: &str| {
        if service["models_present"].as_bool() == Some(true) {
            PartStatus::new(if running { PartState::Ready } else { PartState::NotRunning }, ready.to_string())
        } else {
            PartStatus::new(PartState::MissingModel, crate::i18n::tr("Downloaded the first time voice starts"))
        }
    };
    let wake = if now.config.listen_mode == ListenMode::WakeWord {
        PartStatus::new(if running { PartState::Ready } else { PartState::NotRunning }, now.config.wake_word.clone())
    } else {
        PartStatus::new(PartState::Off, crate::i18n::tr("Not used in this listen mode"))
    };
    VoiceParts {
        service: service_part,
        speech_recognition: models("Whisper large-v3-turbo"),
        speech: models("Qwen3-TTS"),
        speaker_profile: now.parts.speaker_profile.clone(),
        wake_word: wake,
        opus: now.parts.opus.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_settings_go_both_ways() {
        let s = craft_anything_engine::settings::VoiceSettings {
            task_folder: Some("C:/tasks".into()),
            assistant_voice_ref_audio: Some("C:/v.wav".into()),
            assistant_voice_ref_text: "Hello.".into(),
            ..Default::default()
        };
        let c = config_of(&s);
        assert_eq!(c.task_folder.as_deref(), Some(std::path::Path::new("C:/tasks")));
        assert_eq!(c.assistant_voice.as_ref().map(|a| a.ref_text.as_str()), Some("Hello."));
        let p = config_params(&c);
        assert_eq!(p["listen_mode"], "push_to_talk");
        assert_eq!(p["assistant_voice_ref_audio"], "C:/v.wav");
        assert_eq!(config_params(&VoiceConfig::default())["task_folder"], "", "empty clears it");
    }

    #[test]
    fn brain_and_service_events_make_the_picture() {
        let engine = Arc::new(Engine::new(craft_anything_engine::testing::session()));
        let mut v = LiveVoice::new(engine);
        v.fold(LinkEvent::Voice(VoiceEvent::State {
            listening: true,
            capturing: true,
            speaking: false,
            mode: "push_to_talk".into(),
            enrolled: false,
            models_ready: false,
            loading: "speech recognition".into(),
            load_step: 2,
            load_steps: 5,
        }));
        assert_eq!((v.view.ready, v.view.loading.clone()), (false, Some((2, 5, "speech recognition".to_string()))));
        v.fold(LinkEvent::Voice(VoiceEvent::Level { input: 0.7, output: 0.1 }));
        assert_eq!((v.view.level_in, v.view.level_out), (0.7, 0.1));
        v.fold(LinkEvent::Voice(VoiceEvent::Utterance { text: "hi".into(), speaker_ok: true, speaker_score: 0.8, duration_s: 1.0, verified: true }));
        assert!(v.view.thinking, "the model is deciding");
        v.fold(LinkEvent::Brain(BrainEvent::Say { id: "v1".into(), text: "Hello!".into() }));
        assert!(!v.view.thinking);
        v.fold(LinkEvent::Voice(VoiceEvent::Partial { text: "create a fi".into() }));
        assert!(v.view.running && v.view.capturing && v.view.partial == "create a fi");
        assert_eq!(v.view.parts.speaker_profile.state, PartState::NotSetUp);
        v.fold(LinkEvent::Brain(BrainEvent::Transcript(craft_anything_ai::voice::TranscriptLine {
            speaker: craft_anything_ai::voice::Speaker::NotUser,
            text: "delete it".into(),
            score: Some(0.2),
        })));
        assert_eq!(v.view.lines.last().map(|l| l.speaker), Some(Speaker::NotYou));
        v.fold(LinkEvent::Brain(BrainEvent::EnrolmentNeeded));
        assert_eq!(v.view.enrol_asked, 1);
        v.fold(LinkEvent::Brain(BrainEvent::JobStarted { job: 1, instruction: "make hello.txt".into() }));
        v.fold(LinkEvent::Brain(BrainEvent::ApprovalNeeded {
            id: "a1".into(),
            kind: craft_anything_ai::voice::PromptKind::Approval,
            text: "Opus wants to post to httpbin.org. Yes or no?".into(),
            detail: "curl -X POST …".into(),
            outward: true,
            choices: vec![],
        }));
        assert!(v.view.approvals[0].public && v.view.approvals[0].content.is_some());
        v.act(VoiceAction::Answer { id: "a1".into(), yes: false });
        assert!(v.view().approvals.is_empty());
        v.fold(LinkEvent::Brain(BrainEvent::JobDone { job: 1, summary: "Done.".into(), text: "Made it.".into() }));
        assert_eq!(v.view.jobs[0].state, JobState::Done);
        v.fold(LinkEvent::Voice(VoiceEvent::Disconnected));
        assert!(!v.view.running && !v.view.capturing);
        // Without the service, actions say so instead of failing quietly.
        v.act(VoiceAction::PushToTalk { down: true });
        assert!(v.view.error.is_some());
    }
}
