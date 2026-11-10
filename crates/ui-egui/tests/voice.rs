//! Voice UI: the microphone button, push to talk, the conversation (the console's Voice tab and
//! the panel that slides in) and Settings → Voice, against the simulated stand-in.

use craft_anything_engine::Mode;
use craft_anything_ui_egui::state::{BottomTab, SettingsTab};
use craft_anything_ui_egui::voice_demo::{DemoVoice, sample_approval};
use craft_anything_ui_egui::voice_ui::{Enrolment, JobState, ListenMode, OpusJob, Speaker, VoiceAction, VoiceLine};
use craft_anything_ui_egui::{CraftApp, demo, theme};
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};
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
/// chosen.
#[test]
fn the_push_to_talk_key_captures_while_held() {
    let voice = DemoVoice::simulated();
    let mut h = harness(Mode::Experienced, &voice);
    h.run_steps(3);
    h.state_mut().voice.act(VoiceAction::Start);
    h.run_steps(2);
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
    h.run_steps(8);
    h.get_by_label("Enrol my voice").click();
    h.run_steps(3);
    h.get_by_label_contains("1. Craft Anything, please open my newest project");
    h.get_by_label_contains("4. Seven green bottles");
    h.get_by_label("Start recording").scroll_to_me();
    h.run_steps(8);
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
    h.run_steps(8);
    let profile = h.get_by_label("Speaker profile").rect();
    assert!(painted_on_row(&h, profile).iter().any(|t| t == "Ready"));

    // Design the assistant's voice: describe, preview, keep.
    h.get_by_label("Describe the voice").scroll_to_me();
    h.run_steps(8);
    h.get_by_label("Describe the voice").click();
    h.run_steps(8);
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

// ---- Never a dead end, auto-start and the voice orb ----

fn harness_with(mode: Mode, voice: &DemoVoice, auto_start: bool) -> Harness<'static, CraftApp> {
    let voice = voice.clone();
    Harness::builder().with_size(egui::vec2(1280.0, 1000.0)).with_max_steps(32).build_eframe(move |cc| {
        CraftApp::setup_context(&cc.egui_ctx);
        let mut app = demo::app(demo::engine(mode, true));
        app.voice.set_backend(Box::new(voice));
        app.voice.auto_start_allowed = auto_start;
        app
    })
}

/// The orb's label ("Voice: …") and the text painted on it.
fn orb(h: &Harness<'static, CraftApp>) -> Option<(String, Vec<String>)> {
    let found = h
        .query_all_by_label_contains("Voice: ")
        .filter(|n| n.rect().bottom() > 900.0)
        .map(|n| (n.accesskit_node().label().unwrap_or_default().to_string(), n.rect()))
        .next();
    found.map(|(label, rect)| (label, painted_in(h, rect)))
}

fn orb_rect(h: &Harness<'static, CraftApp>) -> egui::Rect {
    h.query_all_by_label_contains("Voice: ").map(|n| n.rect()).find(|r| r.bottom() > 900.0).expect("the orb")
}

/// Pressing F9 while voice is off starts it and says so at once; held until it's ready, it talks.
#[test]
fn f9_while_voice_is_off_starts_it_and_talks_once_ready() {
    let voice = DemoVoice::slow_start(4);
    let mut h = harness_with(Mode::Experienced, &voice, false);
    h.run_steps(3);
    assert!(orb(&h).is_none(), "voice is off: no orb");
    h.key_down(egui::Key::F9);
    h.run_steps(1);
    assert_eq!(voice.actions(), vec![VoiceAction::Start]);
    // (A floating area is laid out unseen on its first frame.)
    h.run_steps(1);
    let (label, painted) = orb(&h).expect("the orb shows at once");
    assert!(label.starts_with("Voice: Starting voice"), "{label}");
    assert!(painted.iter().any(|t| t.starts_with("Starting voice")), "{painted:?}");
    assert!(painted_in(&h, egui::Rect::EVERYTHING).iter().any(|t| t == "Starting voice…"), "the notice says so");
    // Loading, then ready: the key is still held, so it captures.
    h.run_steps(6);
    assert!(h.state().voice.view.capturing, "{:?}", voice.actions());
    assert_eq!(voice.actions(), vec![VoiceAction::Start, VoiceAction::PushToTalk { down: true }]);
    h.key_up(egui::Key::F9);
    h.run_steps(2);
    assert_eq!(voice.actions().last(), Some(&VoiceAction::PushToTalk { down: false }));
}

/// A tap while off starts voice; once ready it says how to talk, and doesn't capture.
#[test]
fn a_tap_while_off_starts_voice_and_says_how_to_talk() {
    let voice = DemoVoice::slow_start(3);
    let mut h = harness_with(Mode::Beginner, &voice, false);
    h.run_steps(3);
    h.key_down(egui::Key::F9);
    h.run_steps(1);
    h.key_up(egui::Key::F9);
    h.run_steps(6);
    assert_eq!(voice.actions(), vec![VoiceAction::Start], "no talking without the key");
    assert!(h.state().voice.view.running);
    assert!(painted_in(&h, egui::Rect::EVERYTHING).iter().any(|t| t == "Voice is on — hold F9 and talk"), "it says how");
    let (label, _) = orb(&h).unwrap();
    assert_eq!(label, "Voice: Ready · hold F9 to talk");
}

/// F9 isn't a text key: it talks even while a text box has the keyboard.
#[test]
fn f9_talks_while_a_text_box_is_focused() {
    let voice = DemoVoice::simulated();
    let mut h = harness_with(Mode::Beginner, &voice, false);
    h.run_steps(3);
    h.state_mut().voice.act(VoiceAction::Start);
    h.state_mut().state.screen = craft_anything_ui_egui::state::Screen::Ask;
    h.run_steps(3);
    let field = h.query_all_by_role(egui::accesskit::Role::TextInput).next().or_else(|| h.query_all_by_role(egui::accesskit::Role::MultilineTextInput).next());
    field.expect("a text box").focus();
    h.run_steps(2);
    assert!(h.ctx.memory(|m| m.focused().is_some()), "the text box has the keyboard");
    h.key_down(egui::Key::F9);
    h.run_steps(2);
    assert!(h.state().voice.view.capturing);
    h.key_up(egui::Key::F9);
    h.run_steps(2);
    assert!(!h.state().voice.view.capturing);
}

/// Voice starts with the app when the settings say so, with a notice the first time; not when
/// they don't.
#[test]
fn voice_starts_with_the_app_when_set_to() {
    let voice = DemoVoice::simulated();
    let mut h = harness_with(Mode::Beginner, &voice, true);
    h.run_steps(4);
    assert_eq!(voice.actions(), vec![VoiceAction::Start]);
    assert!(h.state().voice.auto_start_noticed);
    assert!(painted_in(&h, egui::Rect::EVERYTHING).iter().any(|t| t.starts_with("Voice starts with Craft Anything")));
    assert!(h.state().prefs().voice_auto_start_noticed, "remembered for next time");
    let off = DemoVoice::simulated();
    off.with(|v| v.config.auto_start = false);
    let mut h = harness_with(Mode::Beginner, &off, true);
    h.run_steps(4);
    assert!(off.actions().is_empty());
    assert!(orb(&h).is_none());
}

/// The total height of the upright bars painted in `rect` (the orb's level bars).
fn bars_height(h: &Harness<'static, CraftApp>, rect: egui::Rect) -> f32 {
    fn walk(s: &egui::Shape, rect: egui::Rect, sum: &mut f32) {
        match s {
            egui::Shape::LineSegment { points, .. } if rect.contains(points[0]) && (points[0].x - points[1].x).abs() < 0.1 => {
                *sum += (points[0].y - points[1].y).abs();
            }
            egui::Shape::Vec(v) => v.iter().for_each(|x| walk(x, rect, sum)),
            _ => {}
        }
    }
    let mut sum = 0.0;
    for cs in &h.output().shapes {
        walk(&cs.shape, rect, &mut sum);
    }
    sum
}

/// Each state of the orb is painted with its words, and its bars follow the real loudness.
#[test]
fn the_orb_shows_each_state() {
    let voice = DemoVoice::simulated();
    let mut h = harness_with(Mode::Beginner, &voice, false);
    h.run_steps(3);
    h.state_mut().voice.act(VoiceAction::Start);
    h.run_steps(3);
    let says = |h: &Harness<'static, CraftApp>, words: &str| {
        let (label, painted) = orb(h).expect("the orb");
        assert_eq!(label, format!("Voice: {words}"));
        assert!(painted.iter().any(|t| t == words), "painted {words:?}: {painted:?}");
    };
    says(&h, "Ready · hold F9 to talk");
    // Hearing: the partial text, and bars that grow with the microphone's level.
    voice.with(|v| {
        v.capturing = true;
        v.partial = "make the music louder".into();
        v.level_in = 0.05;
    });
    h.run_steps(2);
    says(&h, "make the music louder");
    // Past the flash that marks the start of hearing.
    std::thread::sleep(std::time::Duration::from_millis(600));
    h.run_steps(30);
    let quiet = bars_height(&h, orb_rect(&h));
    voice.with(|v| v.level_in = 0.95);
    h.run_steps(30);
    let loud = bars_height(&h, orb_rect(&h));
    assert!(quiet > 0.0 && loud > quiet * 1.8, "the bars follow the level: {quiet} → {loud}");
    // Thinking, Opus working, speaking.
    voice.with(|v| {
        v.capturing = false;
        v.partial.clear();
        v.thinking = true;
    });
    h.run_steps(2);
    says(&h, "Thinking…");
    voice.with(|v| {
        v.thinking = false;
        v.jobs.push(OpusJob {
            id: "j1".into(),
            task: "Cut the trailer".into(),
            progress: None,
            latest: String::new(),
            started: Instant::now(),
            state: JobState::Running,
        });
    });
    h.run_steps(2);
    let (label, _) = orb(&h).unwrap();
    assert!(label.starts_with("Voice: Opus is working on: Cut the trailer · 0:0"), "{label}");
    voice.with(|v| {
        v.speaking = true;
        v.level_out = 0.8;
    });
    h.run_steps(2);
    says(&h, "Speaking · tap or press F9 to interrupt");
    // A click interrupts.
    h.get_by_label("Voice: Speaking · tap or press F9 to interrupt").click();
    h.run_steps(2);
    assert_eq!(voice.actions().last(), Some(&VoiceAction::StopSpeaking));
    // A question: the card above the orb, with Yes and No.
    voice.with(|v| {
        v.speaking = false;
        v.approvals.push(sample_approval("a1"));
    });
    h.run_steps(2);
    says(&h, "Waiting for your yes or no");
    h.get_by_label("Opus wants to delete 3 files in Downloads. Yes or no?");
    h.get_by_label("No").click();
    h.run_steps(2);
    assert_eq!(voice.actions().last(), Some(&VoiceAction::Answer { id: "a1".into(), yes: false }));
    // Someone else's voice: a grey shake.
    voice.with(|v| v.lines.push(VoiceLine { speaker: Speaker::NotYou, text: "delete it".into(), speaker_score: Some(0.1) }));
    h.run_steps(2);
    says(&h, "Not your voice");
    // A problem: red, with the message.
    voice.with(|v| v.error = Some("The microphone is unavailable".into()));
    h.run_steps(2);
    says(&h, "The microphone is unavailable");
}
