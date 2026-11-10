//! The voice service and the brain, joined: what the service hears goes to the brain (only the
//! utterances; the brain decides what each one is), what the brain says is spoken by the service,
//! and both sides' events are handed on for the screen ([`LinkEvent`]). It runs on a thread of its
//! own; nothing here blocks the caller.

use super::{BrainEvent, Utterance, VoiceBrain};
use craft_anything_engines::voice::{VoiceClient, VoiceEvent};
use craft_anything_platform::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, PoisonError};
use std::time::Duration;

/// Something that happened, for the screen.
#[derive(Debug, Clone, PartialEq)]
pub enum LinkEvent {
    /// The voice service said it (its state, the words heard, enrolment, devices…).
    Voice(VoiceEvent),
    /// The brain said it (lines, jobs, approvals…). Its `Say` has already been sent to be spoken.
    Brain(BrainEvent),
}

/// How often the link looks at the brain when the service says nothing.
const TICK: Duration = Duration::from_millis(60);

type Waker = Arc<Mutex<Option<Arc<dyn Fn() + Send + Sync>>>>;

/// The voice service and a brain, joined (see the module docs).
pub struct VoiceLink {
    client: Arc<VoiceClient>,
    brain: Arc<Mutex<VoiceBrain>>,
    rx: Receiver<LinkEvent>,
    stop: Arc<AtomicBool>,
    wake: Waker,
}

impl VoiceLink {
    /// Join `client` and `brain`, from now on.
    pub fn start(client: Arc<VoiceClient>, brain: VoiceBrain) -> VoiceLink {
        let brain = Arc::new(Mutex::new(brain));
        let (out, rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let wake: Waker = Arc::default();
        let heard = client.events();
        let (c, b, s, w) = (client.clone(), brain.clone(), stop.clone(), wake.clone());
        let _ = std::thread::Builder::new().name("voice-link".into()).spawn(move || pump(&c, &b, &heard, &out, &s, &w));
        VoiceLink { client, brain, rx, stop, wake }
    }

    /// Everything that happened since the last call.
    pub fn poll(&self) -> Vec<LinkEvent> {
        self.rx.try_iter().collect()
    }

    /// The voice service.
    pub fn client(&self) -> &Arc<VoiceClient> {
        &self.client
    }

    /// Do something with the brain (answer, cancel, read its status). Its calls don't block.
    pub fn brain<T>(&self, f: impl FnOnce(&VoiceBrain) -> T) -> T {
        f(&self.brain.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// Called after every batch of events, so a screen can redraw.
    pub fn set_waker(&self, wake: Arc<dyn Fn() + Send + Sync>) {
        *self.wake.lock().unwrap_or_else(PoisonError::into_inner) = Some(wake);
    }
}

impl Drop for VoiceLink {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.brain(|b| b.stop());
    }
}

/// The link's thread: until stopped, or until the service is gone and the brain has stopped.
fn pump(
    client: &VoiceClient,
    brain: &Mutex<VoiceBrain>,
    heard: &Receiver<VoiceEvent>,
    out: &std::sync::mpsc::Sender<LinkEvent>,
    stop: &AtomicBool,
    wake: &Waker,
) {
    let mut service_gone = false;
    let mut brain_stopped = false;
    while !stop.load(Ordering::SeqCst) && !(service_gone && brain_stopped) {
        let mut any = false;
        let first = if service_gone {
            std::thread::sleep(TICK);
            None
        } else {
            match heard.recv_timeout(TICK) {
                Ok(e) => Some(e),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => {
                    service_gone = true;
                    None
                }
            }
        };
        for ev in first.into_iter().chain(heard.try_iter()) {
            any = true;
            match &ev {
                VoiceEvent::Utterance { text, speaker_ok, speaker_score, duration_s, verified } => {
                    let u =
                        Utterance { text: text.clone(), speaker_ok: *speaker_ok, speaker_score: *speaker_score, duration_s: *duration_s, verified: *verified };
                    brain.lock().unwrap_or_else(PoisonError::into_inner).utterance(u);
                }
                VoiceEvent::Disconnected => service_gone = true,
                _ => {}
            }
            let _ = out.send(LinkEvent::Voice(ev));
        }
        let said = brain.lock().unwrap_or_else(PoisonError::into_inner).poll();
        for ev in said {
            any = true;
            match &ev {
                BrainEvent::Say { id, text } if !service_gone => {
                    if let Err(e) = client.speak(id, text) {
                        log::info!("voice: couldn't speak: {}", e.message);
                    }
                }
                BrainEvent::Stopped => brain_stopped = true,
                _ => {}
            }
            let _ = out.send(LinkEvent::Brain(ev));
        }
        if any && let Some(w) = wake.lock().unwrap_or_else(PoisonError::into_inner).clone() {
            w();
        }
    }
}
