//! Voice UI: the microphone button, push to talk, the conversation (the console's Voice tab and
//! the panel that slides in) and Settings → Voice, against the simulated stand-in.

use craft_anything_engine::Mode;
use craft_anything_ui_egui::state::{BottomTab, SettingsTab};
use craft_anything_ui_egui::voice_demo::{DemoVoice, sample_approval};
use craft_anything_ui_egui::voice_ui::{Enrolment, JobState, ListenMode, OpusJob, Speaker, VoiceAction, VoiceLine};
use craft_anything_ui_egui::{CraftApp, demo, theme};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use std::time::Instant;

fn harness(mode: Mode, voice: &DemoVoice) -> Harness<'static, CraftApp> {
    let voice = voice.clone();
    Harness::builder().with_size(egui::vec2(1280.0, 1000.0)).with_max_steps(32).build_eframe(move |cc| {
        CraftApp::setup_context(&cc.egui_ctx);
        let mut app = demo::app(demo::engine(mode, true));
        app.voice.set_backend(Box::new(voice));
        app
    })
}

/// The text painted inside `rect` (what's on screen, not only what screen readers are told).
fn painted_in(h: &Harness<'static, CraftApp>, rect: egui::Rect) -> Vec<String> {
    h.output()
        .shapes
        .iter()
        .filter_map(|cs| match &cs.shape {
            egui::Shape::Text(t) if t.visual_bounding_rect().intersects(rect) => Some(t.galley.text().to_owned()),
            _ => None,
        })
        .collect()
}

/// The text painted on the same line as `rect`, across the window.
fn painted_on_row(h: &Harness<'static, CraftApp>, rect: egui::Rect) -> Vec<String> {
    painted_in(h, egui::Rect::from_x_y_ranges(0.0..=4000.0, rect.shrink2(egui::vec2(0.0, 2.0)).y_range()))
}

/// The colour a painted text was drawn in.
fn painted_colour(h: &Harness<'static, CraftApp>, text: &str) -> Option<egui::Color32> {
    h.output().shapes.iter().find_map(|cs| match &cs.shape {
        egui::Shape::Text(t) if t.galley.text() == text => t.galley.job.sections.first().map(|s| s.format.color),
        _ => None,
    })
}

fn mic_rect(h: &Harness<'static, CraftApp>, state: &str) -> egui::Rect {
    h.get_by_label(&format!("Voice: {state}")).rect()
}

/// Off, ready, hearing you, Opus working and speaking: each is named and painted on the button.
#[test]
fn the_microphone_button_shows_each_state() {
    let voice = DemoVoice::simulated();
    let mut h = harness(Mode::Beginner, &voice);
    h.run_steps(3);
    // Off: only the icon; nothing painted on the button.
    let off = mic_rect(&h, "Off");
    assert!(painted_in(&h, off).is_empty(), "{:?}", painted_in(&h, off));
    h.get_by_label("Voice: Off").click();
    h.run_steps(3);
    assert_eq!(voice.actions(), vec![VoiceAction::Start]);
    assert!(h.state().voice.view.running);
    // Started from the button: the conversation slides in.
    assert!(h.state().voice.panel_open);
    let ready = mic_rect(&h, "Ready");
    assert!(painted_in(&h, ready).iter().any(|t| t == "Ready"), "{:?}", painted_in(&h, ready));
    // Press to talk, press again to finish.
    h.get_by_label("Voice: Ready").click();
    h.run_steps(3);
    assert!(painted_in(&h, mic_rect(&h, "Hearing you")).iter().any(|t| t == "Hearing you"));
    h.get_by_label("Voice: Hearing you").click();
    h.run_steps(3);
    assert_eq!(voice.actions().get(1..), Some(&[VoiceAction::PushToTalk { down: true }, VoiceAction::PushToTalk { down: false }][..]));
    // What was said went to Opus: the button says it's working.
    assert!(painted_in(&h, mic_rect(&h, "Opus working")).iter().any(|t| t == "Opus working"));
    voice.with(|v| v.speaking = true);
    h.run_steps(3);
    assert!(painted_in(&h, mic_rect(&h, "Speaking")).iter().any(|t| t == "Speaking"));
    h.get_by_label("Voice: Speaking").click();
    h.run_steps(3);
    assert_eq!(voice.actions().last(), Some(&VoiceAction::StopSpeaking));
    // In wake-word mode the ready state is "Listening".
    voice.with(|v| v.config.listen_mode = ListenMode::WakeWord);
    voice.with(|v| v.jobs.clear());
    h.run_steps(3);
    assert!(painted_in(&h, mic_rect(&h, "Listening")).iter().any(|t| t == "Listening"));
}

/// F9 down starts capturing and up ends it; held repeats send nothing more; another key can be
/// chosen; while voice is off the key only says how to start it.
#[test]
fn the_push_to_talk_key_captures_while_held() {
    let voice = DemoVoice::simulated();
    let mut h = harness(Mode::Experienced, &voice);
    h.run_steps(3);
    h.key_down(egui::Key::F9);
    h.run_steps(2);
    h.key_up(egui::Key::F9);
    h.run_steps(2);
    assert!(voice.actions().is_empty(), "voice is off: {:?}", voice.actions());
    assert!(h.state().state.toasts.iter().any(|(t, _)| t.contains("Voice is off")));
    h.state_mut().voice.act(VoiceAction::Start);
    h.key_down(egui::Key::F9);
    h.run_steps(2);
    assert!(h.state().voice.view.capturing);
    assert!(h.state().voice.talking());
    // The partial text shows while talking (the Voice tab).
    h.state_mut().state.bottom = BottomTab::Voice;
    h.run_steps(2);
    h.get_by_label("open the trailer fol…");
    h.key_up(egui::Key::F9);
    h.run_steps(2);
    assert!(!h.state().voice.view.capturing);
    assert_eq!(voice.actions(), vec![VoiceAction::Start, VoiceAction::PushToTalk { down: true }, VoiceAction::PushToTalk { down: false }]);
    h.get_by_label("Open the trailer folder.");
    // Another key: F9 does nothing now, F8 talks.
    h.state_mut().voice.configure(|c| c.push_to_talk_key = "F8".into());
    let before = voice.actions().len();
    h.key_down(egui::Key::F9);
    h.run_steps(2);
    h.key_up(egui::Key::F9);
    h.run_steps(2);
    assert_eq!(voice.actions().len(), before);
    h.key_down(egui::Key::F8);
    h.run_steps(2);
    assert!(h.state().voice.view.capturing);
    h.key_up(egui::Key::F8);
    h.run_steps(2);
    assert!(!h.state().voice.view.capturing);
    // A tap quicker than a frame still ends what it started.
    let before = voice.actions().len();
    h.key_down(egui::Key::F8);
    h.key_up(egui::Key::F8);
    h.run_steps(2);
    assert_eq!(voice.actions().get(before..), Some(&[VoiceAction::PushToTalk { down: true }, VoiceAction::PushToTalk { down: false }][..]));
    assert!(!h.state().voice.view.capturing && !h.state().voice.talking());
}

/// The console's Voice tab: what's being said, the conversation (other voices greyed), Opus's
/// jobs with Stop, and questions with Yes / No that reach the voice parts.
#[test]
fn the_voice_tab_shows_the_conversation_jobs_and_questions() {
    let voice = DemoVoice::simulated();
    let mut h = harness(Mode::Experienced, &voice);
    h.run_steps(3);
    h.state_mut().voice.act(VoiceAction::Start);
    voice.with(|v| {
        v.enrolled = true;
        v.capturing = true;
        v.partial = "and then post it".into();
        v.lines = vec![
            VoiceLine { speaker: Speaker::You, text: "Make the trailer louder.".into(), speaker_score: Some(0.8) },
            VoiceLine { speaker: Speaker::NotYou, text: "Buy two tickets now.".into(), speaker_score: Some(0.2) },
            VoiceLine { speaker: Speaker::Assistant, text: "Opus is raising the music.".into(), speaker_score: None },
        ];
        v.jobs = vec![OpusJob {
            id: "job-7".into(),
            task: "Raise the trailer's music".into(),
            progress: Some(0.4),
            latest: "Mixing the second clip".into(),
            started: Instant::now(),
            state: JobState::Running,
        }];
        v.approvals = vec![sample_approval("ask-1")];
    });
    h.state_mut().state.bottom = BottomTab::Voice;
    h.run_steps(3);
    // The tab counts the waiting question, on screen too.
    let tab = h.get_by_label("Voice (1)").rect();
    assert!(painted_in(&h, tab).iter().any(|t| t == "Voice (1)"), "{:?}", painted_in(&h, tab));
    h.get_by_label("and then post it…");
    h.get_by_label("Make the trailer louder.");
    h.get_by_label("Opus is raising the music.");
    h.get_by_label("Opus is working on…");
    h.get_by_label("Raise the trailer's music");
    h.get_by_label("Mixing the second clip");
    h.get_by_label("Opus wants to delete 3 files in Downloads. Yes or no?");
    // Another voice is greyed and named as such.
    h.get_by_label("Buy two tickets now.");
    let faint = theme::Tokens::get(&h.ctx).text_faint;
    assert_eq!(painted_colour(&h, "Buy two tickets now."), Some(faint));
    assert_eq!(painted_colour(&h, "Not your voice"), Some(faint));
    assert_ne!(painted_colour(&h, "Make the trailer louder."), Some(faint));
    // Stop reaches the job.
    h.get_by_label("Stop").click();
    h.run_steps(3);
    assert!(voice.actions().contains(&VoiceAction::CancelJob { id: "job-7".into() }));
    h.get_by_label("Stopped");
    // Yes answers the question; the count goes.
    h.get_by_label("Yes").click();
    h.run_steps(3);
    assert!(voice.actions().contains(&VoiceAction::Answer { id: "ask-1".into(), yes: true }));
    assert!(h.query_by_label("Opus wants to delete 3 files in Downloads. Yes or no?").is_none());
    let tab = h.get_by_label("Voice").rect();
    assert!(painted_in(&h, tab).iter().any(|t| t == "Voice"));
    // A public action shows what goes out; No refuses it.
    voice.with(|v| {
        let mut a = sample_approval("ask-2");
        a.public = true;
        a.question = "Opus wants to post the trailer to your channel. Yes or no?".into();
        a.content = Some("New trailer! Out now.".into());
        v.approvals.push(a);
    });
    h.run_steps(3);
    h.get_by_label("Opus asks before anything public");
    h.get_by_label("New trailer! Out now.");
    h.get_by_label("No").click();
    h.run_steps(3);
    assert!(voice.actions().contains(&VoiceAction::Answer { id: "ask-2".into(), yes: false }));
    // No control lets the local model do the work.
    assert!(h.query_by_label_contains("local model").is_none());
}

/// Off the Workbench, the conversation slides in from the right and closes again.
#[test]
fn the_voice_panel_slides_in_on_simple_screens() {
    let voice = DemoVoice::simulated();
    let mut h = harness(Mode::Beginner, &voice);
    h.run_steps(3);
    assert!(h.query_by_label("Close the voice panel").is_none());
    h.get_by_label("Voice: Off").click();
    h.run_steps(10);
    h.get_by_label("Close the voice panel");
    h.get_by_label("Stop voice");
    // Not enrolled: the panel says anyone's voice could give commands.
    h.get_by_label_contains("Your voice isn't enrolled yet");
    voice.with(|v| v.approvals.push(sample_approval("ask-9")));
    h.run_steps(3);
    h.get_by_label("Yes").click();
    h.run_steps(3);
    assert!(voice.actions().contains(&VoiceAction::Answer { id: "ask-9".into(), yes: true }));
    h.get_by_label("Close the voice panel").click();
    h.run_steps(10);
    assert!(!h.state().voice.panel_open);
    assert!(h.query_by_label("Close the voice panel").is_none());
    // The conversation button next to the microphone brings it back.
    h.get_by_label("Voice conversation").click();
    h.run_steps(10);
    assert!(h.state().voice.panel_open);
}

/// Settings → Voice: every part's status (painted), Start, the listen mode, and the enrol and
/// design flows.
#[test]
fn settings_voice_shows_the_parts_and_enrols() {
    let voice = DemoVoice::simulated();
    let mut h = harness(Mode::Beginner, &voice);
    h.state_mut().state.settings_open = true;
    h.state_mut().state.settings_tab = SettingsTab::Voice;
    h.run_steps(3);
    for name in ["Voice service", "Speech recognition", "Speech", "Speaker profile", "Wake word", "Opus"] {
        h.get_by_label(name);
    }
    let service = h.get_by_label("Voice service").rect();
    assert!(painted_on_row(&h, service).iter().any(|t| t == "Not running"), "{:?}", painted_on_row(&h, service));
    h.get_by_label("Start voice").click();
    h.run_steps(3);
    assert_eq!(voice.actions(), vec![VoiceAction::Start]);
    let service = h.get_by_label("Voice service").rect();
    assert!(painted_on_row(&h, service).iter().any(|t| t == "Ready"), "{:?}", painted_on_row(&h, service));
    let wake = h.get_by_label("Wake word").rect();
    assert!(painted_on_row(&h, wake).iter().any(|t| t == "Off"), "{:?}", painted_on_row(&h, wake));
    let profile = h.get_by_label("Speaker profile").rect();
    assert!(painted_on_row(&h, profile).iter().any(|t| t == "Not set up"));
    // The wake word mode turns the wake word on.
    h.get_by_label("After the wake word").click();
    h.run_steps(3);
    assert_eq!(h.state().voice.view.config.listen_mode, ListenMode::WakeWord);
    assert!(voice.actions().iter().any(|a| matches!(a, VoiceAction::SetConfig(c) if c.listen_mode == ListenMode::WakeWord)));
    let wake = h.get_by_label("Wake word").rect();
    assert!(painted_on_row(&h, wake).iter().any(|t| t == "Ready"), "{:?}", painted_on_row(&h, wake));

    // Enrol: four sentences, progress, too little, then enough.
    h.get_by_label("Enrol my voice").scroll_to_me();
    h.run();
    h.get_by_label("Enrol my voice").click();
    h.run_steps(3);
    h.get_by_label_contains("1. Craft Anything, please open my newest project");
    h.get_by_label_contains("4. Seven green bottles");
    h.get_by_label("Start recording").scroll_to_me();
    h.run();
    h.get_by_label("Start recording").click();
    h.run_steps(3);
    assert_eq!(voice.actions().last(), Some(&VoiceAction::EnrolStart));
    voice.hear(12.0);
    h.run_steps(3);
    let said = h.get_by_label("12 of 20 seconds").rect();
    assert!(painted_in(&h, said).iter().any(|t| t == "12 of 20 seconds"));
    h.get_by_label("Stop").click();
    h.run_steps(3);
    assert_eq!(h.state().voice.view.enrolment, Enrolment::Finished { ok: false });
    h.get_by_label_contains("That wasn't enough of your voice");
    h.get_by_label("Start recording").click();
    h.run_steps(3);
    voice.hear(21.0);
    h.run_steps(3);
    h.get_by_label("Finish").click();
    h.run_steps(3);
    assert!(h.state().voice.view.enrolled);
    h.get_by_label("Done. Your voice is enrolled.");
    h.get_by_label("Speaker profile").scroll_to_me();
    h.run();
    let profile = h.get_by_label("Speaker profile").rect();
    assert!(painted_on_row(&h, profile).iter().any(|t| t == "Ready"));

    // Design the assistant's voice: describe, preview, keep.
    h.get_by_label("Describe the voice").scroll_to_me();
    h.run();
    h.get_by_label("Describe the voice").click();
    h.run();
    h.get_by_label("Describe the voice").type_text("A calm, deep voice");
    h.run_steps(3);
    h.get_by_label("Preview").click();
    h.run_steps(3);
    assert!(voice.actions().iter().any(|a| matches!(a, VoiceAction::DesignVoice { description, .. } if description == "A calm, deep voice")));
    h.get_by_label("Use this voice").click();
    h.run_steps(3);
    assert!(h.state().voice.view.config.assistant_voice.is_some());
    h.get_by_label("It speaks with the voice you designed.");
}

/// Without the voice service in the build, Start says so instead of pretending.
#[test]
fn the_unconnected_stand_in_says_voice_is_not_here_yet() {
    let voice = DemoVoice::unconnected();
    let mut h = harness(Mode::Beginner, &voice);
    h.run_steps(3);
    h.get_by_label("Voice: Off").click();
    h.run_steps(5);
    assert!(!h.state().voice.view.running);
    h.get_by_label_contains("Voice isn't connected in this build yet");
}
