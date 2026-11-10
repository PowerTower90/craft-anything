//! A stand-in for the voice parts until the voice service and the brain are wired in.
//!
//! [`DemoVoice::unconnected`] is what the app uses meanwhile: it shows every part as not set up
//! and says so when Start is pressed. [`DemoVoice::simulated`] answers at once like a working
//! service (UI tests and screenshots): talking adds a line, Opus gets a job, questions are
//! answered. Clones share one state, so a test keeps a clone to look at what the UI asked for
//! ([`DemoVoice::actions`]) and to change what it shows ([`DemoVoice::with`]).

use crate::i18n::tr;
use crate::voice_ui::{
    AssistantVoice, Enrolment, JobState, ListenMode, OpusJob, PartState, PartStatus, Speaker, VoiceAction, VoiceApproval, VoiceBackend, VoiceDesign, VoiceLine,
    VoiceParts, VoiceView,
};
use craft_anything_platform::sync::Mutex;
use std::path::PathBuf;
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

/// Seconds of speech the stand-in's enrolment asks for (the contract's 20).
pub const ENROL_SECONDS: f32 = 20.0;

struct State {
    view: VoiceView,
    simulated: bool,
    actions: Vec<VoiceAction>,
    wake: Option<Arc<dyn Fn() + Send + Sync>>,
    next_job: u64,
    /// A start takes this many more looks before the service is ready (0: at once).
    start_views: u32,
}

/// The stand-in voice parts. See the module docs.
#[derive(Clone)]
pub struct DemoVoice {
    state: Arc<Mutex<State>>,
}

impl DemoVoice {
    fn new(simulated: bool) -> Self {
        let parts = if simulated { stopped_parts(false, ListenMode::PushToTalk) } else { VoiceParts::default() };
        let view = VoiceView { parts, ..Default::default() };
        DemoVoice { state: Arc::new(Mutex::new(State { view, simulated, actions: Vec::new(), wake: None, next_job: 1, start_views: 0 })) }
    }

    /// Not connected to anything: Start explains that voice isn't in this build yet.
    pub fn unconnected() -> Self {
        Self::new(false)
    }

    /// Behaves like a working voice service and brain, at once and without a microphone.
    pub fn simulated() -> Self {
        Self::new(true)
    }

    /// A simulated service that is running, with a conversation, a job and a question: for
    /// screenshots.
    pub fn filled() -> Self {
        let mut d = Self::simulated();
        d.act(VoiceAction::Start);
        d.act(VoiceAction::RefreshDevices);
        d.with(|v| {
            v.enrolled = true;
            v.parts.speaker_profile = PartStatus::new(PartState::Ready, "Enrolled");
            v.capturing = true;
            v.partial = "and make the music a bit louder".into();
            v.lines = vec![
                VoiceLine { speaker: Speaker::You, text: "Cut the trailer to sixty seconds.".into(), speaker_score: Some(0.78) },
                VoiceLine { speaker: Speaker::Assistant, text: "Opus is on it. I'll tell you when it's done.".into(), speaker_score: None },
                VoiceLine { speaker: Speaker::NotYou, text: "Buy two tickets for tonight.".into(), speaker_score: Some(0.21) },
            ];
            v.jobs = vec![OpusJob {
                id: "job-1".into(),
                task: "Cut the trailer to sixty seconds".into(),
                progress: Some(0.45),
                latest: "Trimming the third clip".into(),
                started: Instant::now(),
                state: JobState::Running,
            }];
            v.approvals = vec![sample_approval("ask-1")];
        });
        d.lock(|s| s.actions.clear());
        d
    }

    /// A simulated service whose start takes `views` looks (frames) before it's ready, loading its
    /// models meanwhile, as the real one does.
    pub fn slow_start(views: u32) -> Self {
        let d = Self::simulated();
        d.lock(|s| s.start_views = views);
        d
    }

    /// Every action the UI asked for, oldest first.
    pub fn actions(&self) -> Vec<VoiceAction> {
        self.lock(|s| s.actions.clone())
    }

    /// Change what the stand-in shows.
    pub fn with<R>(&self, f: impl FnOnce(&mut VoiceView) -> R) -> R {
        let r = self.lock(|s| f(&mut s.view));
        self.wake();
        r
    }

    /// The person spoke for `seconds` while enrolling.
    pub fn hear(&self, seconds: f32) {
        self.with(|v| {
            if let Enrolment::Recording { seconds: s, .. } = &mut v.enrolment {
                *s += seconds.max(0.0);
            }
        });
    }

    fn lock<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        let mut g = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut g)
    }

    fn wake(&self) {
        if let Some(w) = self.lock(|s| s.wake.clone()) {
            w();
        }
    }
}

/// The parts of a simulated service that isn't running.
fn stopped_parts(enrolled: bool, mode: ListenMode) -> VoiceParts {
    let mut p = running_parts(enrolled, mode);
    p.service = PartStatus::new(PartState::NotRunning, "Press Start voice.");
    p.speech_recognition = PartStatus::new(PartState::NotRunning, "Whisper large-v3-turbo");
    p.speech = PartStatus::new(PartState::NotRunning, "Qwen3-TTS 1.7B");
    p
}

fn running_parts(enrolled: bool, mode: ListenMode) -> VoiceParts {
    VoiceParts {
        service: PartStatus::new(PartState::Ready, "ws://127.0.0.1:8191"),
        speech_recognition: PartStatus::new(PartState::Ready, "Whisper large-v3-turbo"),
        speech: PartStatus::new(PartState::Ready, "Qwen3-TTS 1.7B"),
        speaker_profile: if enrolled { PartStatus::new(PartState::Ready, "Enrolled") } else { PartStatus::new(PartState::NotSetUp, "Enrol your voice") },
        wake_word: if mode == ListenMode::WakeWord { PartStatus::new(PartState::Ready, "openWakeWord") } else { PartStatus::new(PartState::Off, "") },
        opus: PartStatus::new(PartState::Ready, "Claude Code · claude-opus-5-5"),
    }
}

fn simulate(s: &mut State, action: &VoiceAction) {
    let v = &mut s.view;
    match action {
        VoiceAction::Start if s.start_views > 0 => {
            v.starting = true;
            v.error = None;
            v.loading = Some((0, 5, "speech recognition".into()));
        }
        VoiceAction::Start => {
            v.running = true;
            v.ready = true;
            v.listening = v.config.listen_mode != ListenMode::PushToTalk;
            v.error = None;
            v.parts = running_parts(v.enrolled, v.config.listen_mode);
        }
        VoiceAction::Stop => {
            v.running = false;
            v.ready = false;
            v.listening = false;
            v.capturing = false;
            v.speaking = false;
            v.partial.clear();
            v.parts = stopped_parts(v.enrolled, v.config.listen_mode);
        }
        VoiceAction::PushToTalk { down: true } if v.running => {
            v.capturing = true;
            v.speaking = false;
            v.level_in = 0.6;
            v.partial = "open the trailer fol".into();
        }
        VoiceAction::PushToTalk { down: false } if v.capturing => {
            v.capturing = false;
            v.level_in = 0.0;
            v.partial.clear();
            if let Enrolment::Recording { seconds, .. } = &mut v.enrolment {
                *seconds += 5.0;
                return;
            }
            v.lines.push(VoiceLine { speaker: Speaker::You, text: "Open the trailer folder.".into(), speaker_score: Some(0.71) });
            v.lines.push(VoiceLine { speaker: Speaker::Assistant, text: "Opus is opening it.".into(), speaker_score: None });
            let id = format!("job-{}", s.next_job);
            s.next_job += 1;
            v.jobs.push(OpusJob {
                id,
                task: "Open the trailer folder".into(),
                progress: None,
                latest: "Starting Claude Code".into(),
                started: Instant::now(),
                state: JobState::Running,
            });
        }
        VoiceAction::PushToTalk { .. } => {}
        VoiceAction::StopSpeaking => v.speaking = false,
        VoiceAction::Answer { id, yes } => {
            if v.approvals.iter().any(|a| &a.id == id) {
                v.approvals.retain(|a| &a.id != id);
                let text = if *yes { "Okay, going ahead." } else { "Okay, I won't." };
                v.lines.push(VoiceLine { speaker: Speaker::Assistant, text: text.into(), speaker_score: None });
            }
        }
        VoiceAction::CancelJob { id } => {
            for j in v.jobs.iter_mut().filter(|j| &j.id == id && j.state == JobState::Running) {
                j.state = JobState::Stopped;
            }
        }
        VoiceAction::EnrolStart if v.running => v.enrolment = Enrolment::Recording { seconds: 0.0, needed: ENROL_SECONDS },
        VoiceAction::EnrolStart => {}
        VoiceAction::EnrolStop => {
            if let Enrolment::Recording { seconds, needed } = v.enrolment {
                let ok = seconds >= needed;
                v.enrolment = Enrolment::Finished { ok };
                v.enrolled |= ok;
                v.parts.speaker_profile = running_parts(v.enrolled, v.config.listen_mode).speaker_profile;
            }
        }
        VoiceAction::DesignVoice { sample_text, .. } if v.running => {
            v.design = VoiceDesign::Ready(AssistantVoice { ref_audio: PathBuf::from("designed-voice.wav"), ref_text: sample_text.clone() });
        }
        VoiceAction::DesignVoice { .. } => {}
        VoiceAction::UseDesignedVoice => {
            if let VoiceDesign::Ready(voice) = &v.design {
                v.config.assistant_voice = Some(voice.clone());
            }
        }
        VoiceAction::SetConfig(c) => {
            v.config = c.clone();
            let running = if v.running { running_parts(v.enrolled, c.listen_mode) } else { stopped_parts(v.enrolled, c.listen_mode) };
            v.parts.wake_word = running.wake_word;
        }
        VoiceAction::RefreshDevices => {
            v.inputs = vec!["Microphone (USB)".into(), "Headset microphone".into()];
            v.outputs = vec!["Speakers".into(), "Headset".into()];
        }
        VoiceAction::CheckStatus => {}
    }
}

/// A question the simulated brain asks, for tests and screenshots.
pub fn sample_approval(id: &str) -> VoiceApproval {
    VoiceApproval {
        id: id.into(),
        question: "Opus wants to delete 3 files in Downloads. Yes or no?".into(),
        content: None,
        public: false,
        deadline: Some(Instant::now() + Duration::from_secs(120)),
    }
}

impl VoiceBackend for DemoVoice {
    fn view(&mut self) -> VoiceView {
        self.lock(|s| {
            // A slow start loads its models a step per look, then is ready.
            if s.view.starting {
                s.start_views = s.start_views.saturating_sub(1);
                if let Some((step, steps, _)) = &mut s.view.loading {
                    *step = (*step + 1).min(*steps);
                }
                if s.start_views == 0 {
                    let v = &mut s.view;
                    (v.starting, v.running, v.ready, v.loading) = (false, true, true, None);
                    v.parts = running_parts(v.enrolled, v.config.listen_mode);
                }
            }
            s.view.clone()
        })
    }

    fn act(&mut self, action: VoiceAction) {
        self.lock(|s| {
            s.actions.push(action.clone());
            if s.simulated {
                simulate(s, &action);
            } else {
                match action {
                    VoiceAction::Start => s.view.error = Some(tr("Voice isn't connected in this build yet: it comes with the voice service.")),
                    VoiceAction::SetConfig(c) => s.view.config = c,
                    _ => {}
                }
            }
        });
        self.wake();
    }

    fn set_waker(&mut self, wake: Arc<dyn Fn() + Send + Sync>) {
        self.lock(|s| s.wake = Some(wake));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_simulated_service_talks_works_and_answers() {
        let mut d = DemoVoice::simulated();
        d.act(VoiceAction::Start);
        d.act(VoiceAction::PushToTalk { down: true });
        assert!(d.view().capturing);
        d.act(VoiceAction::PushToTalk { down: false });
        let v = d.view();
        assert!(!v.capturing);
        assert_eq!(v.lines.first().map(|l| l.speaker), Some(Speaker::You));
        assert_eq!(v.jobs.len(), 1);
        d.act(VoiceAction::CancelJob { id: "job-1".into() });
        assert_eq!(d.view().jobs.first().map(|j| j.state), Some(JobState::Stopped));
        d.with(|v| v.approvals.push(sample_approval("a1")));
        d.act(VoiceAction::Answer { id: "a1".into(), yes: false });
        assert!(d.view().approvals.is_empty());
    }

    #[test]
    fn enrolment_needs_enough_speech() {
        let mut d = DemoVoice::simulated();
        d.act(VoiceAction::Start);
        d.act(VoiceAction::EnrolStart);
        d.hear(8.0);
        d.act(VoiceAction::EnrolStop);
        assert_eq!(d.view().enrolment, Enrolment::Finished { ok: false });
        d.act(VoiceAction::EnrolStart);
        d.hear(21.0);
        d.act(VoiceAction::EnrolStop);
        let v = d.view();
        assert!(v.enrolled);
        assert_eq!(v.parts.speaker_profile.state, PartState::Ready);
    }

    #[test]
    fn unconnected_says_so_and_keeps_settings() {
        let mut d = DemoVoice::unconnected();
        d.act(VoiceAction::Start);
        let v = d.view();
        assert!(!v.running && v.error.is_some());
        let mut c = v.config.clone();
        c.push_to_talk_key = "F8".into();
        d.act(VoiceAction::SetConfig(c));
        assert_eq!(d.view().config.push_to_talk_key, "F8");
    }
}
