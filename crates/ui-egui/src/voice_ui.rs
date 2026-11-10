//! Voice: talking to Craft Anything. The microphone button in the title bar, the push-to-talk key,
//! the conversation (the Workbench console's Voice tab, and a panel that slides in on the other
//! screens) and Settings → Voice.
//!
//! The rules the screens keep (the voice contract): listening, understanding and speaking run on
//! this computer; everything the person asks to be *done* is done by Claude Code with Opus, so the
//! jobs here are always Opus's and no control lets the local model do a task. Only the person's
//! own voice gives commands: other voices are shown greyed and never sent on. Risky and public
//! actions are asked out loud and answered by voice or with the Yes / No buttons here.
//!
//! The UI talks to the voice parts only through [`VoiceBackend`]: a snapshot to draw
//! ([`VoiceBackend::view`]) and commands ([`VoiceBackend::act`]). Until the voice service (the
//! engines crate's `voice` client) and the conversation brain (the AI crate's `voice` module) are
//! wired in, the app uses [`crate::voice_demo::DemoVoice`].

use crate::i18n::{tr, trf};
use crate::theme::{self, Tokens};
use crate::{CraftApp, icons, widgets};
use egui::{Color32, RichText, Ui, Vec2};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

// ---- What the UI needs from the voice parts ---------------------------------------------------

/// When the microphone listens (the service's `listen_mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ListenMode {
    /// Only while the push-to-talk key (or the microphone button) is held down.
    #[default]
    PushToTalk,
    /// After the wake word.
    WakeWord,
    /// All the time; the pauses end each sentence.
    Always,
}

impl ListenMode {
    pub const ALL: [ListenMode; 3] = [ListenMode::PushToTalk, ListenMode::WakeWord, ListenMode::Always];

    /// The id in the protocol and in `config.toml`.
    pub fn id(self) -> &'static str {
        match self {
            ListenMode::PushToTalk => "push_to_talk",
            ListenMode::WakeWord => "wake_word",
            ListenMode::Always => "always",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.id() == id)
    }
}

/// A designed assistant voice: the reference recording and what it says.
#[derive(Debug, Clone, PartialEq)]
pub struct AssistantVoice {
    pub ref_audio: PathBuf,
    pub ref_text: String,
}

/// The voice settings the person can change (`[voice]` in `config.toml`). `confirm_public_actions`
/// isn't here: it is always on and can't be changed.
#[derive(Debug, Clone, PartialEq)]
pub struct VoiceConfig {
    pub listen_mode: ListenMode,
    pub wake_word: String,
    /// An egui key name ("F9"): the key held to talk while the app is focused.
    pub push_to_talk_key: String,
    /// `None`: the system's default device.
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    /// How much an utterance must sound like the enrolled voice to count (0–1).
    pub speaker_threshold: f32,
    /// The folder Opus works in for voice tasks; `None`: the person's home folder.
    pub task_folder: Option<PathBuf>,
    /// `None`: the built-in voice.
    pub assistant_voice: Option<AssistantVoice>,
    /// Start the voice service when the app opens.
    pub auto_start: bool,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        VoiceConfig {
            listen_mode: ListenMode::PushToTalk,
            wake_word: "hey craft".into(),
            push_to_talk_key: "F9".into(),
            input_device: None,
            output_device: None,
            speaker_threshold: 0.55,
            task_folder: None,
            assistant_voice: None,
            auto_start: false,
        }
    }
}

/// How one voice part is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PartState {
    Ready,
    /// Not used with the current settings (the wake word in push-to-talk mode).
    Off,
    /// Set up, but not running now.
    NotRunning,
    /// A model it needs isn't downloaded yet.
    MissingModel,
    #[default]
    NotSetUp,
}

/// One part's status and a line about it in the person's words.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PartStatus {
    pub state: PartState,
    pub detail: String,
}

impl PartStatus {
    pub fn new(state: PartState, detail: impl Into<String>) -> Self {
        PartStatus { state, detail: detail.into() }
    }
}

/// The voice parts Settings → Voice shows a status for.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct VoiceParts {
    /// The voice service (the Python program on this computer).
    pub service: PartStatus,
    /// Speech recognition (Whisper).
    pub speech_recognition: PartStatus,
    /// Speech (Qwen3-TTS).
    pub speech: PartStatus,
    /// The person's enrolled voice (speaker verification).
    pub speaker_profile: PartStatus,
    pub wake_word: PartStatus,
    /// Claude Code with Opus, which does the work.
    pub opus: PartStatus,
}

/// Who said a line of the conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speaker {
    /// The person (their voice matched the enrolled one).
    You,
    /// Craft Anything's spoken answers, and Opus's results.
    Assistant,
    /// Speech that didn't sound like the person: shown greyed, never acted on.
    NotYou,
}

/// One line of the conversation.
#[derive(Debug, Clone, PartialEq)]
pub struct VoiceLine {
    pub speaker: Speaker,
    pub text: String,
    /// How much it sounded like the enrolled voice, when it was heard (0–1).
    pub speaker_score: Option<f32>,
}

/// Where an Opus job is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Running,
    Done,
    Failed,
    Stopped,
}

/// A task Opus is doing (or did) for the person.
#[derive(Debug, Clone, PartialEq)]
pub struct OpusJob {
    pub id: String,
    /// What was asked, in the person's words.
    pub task: String,
    /// 0–1 when known.
    pub progress: Option<f32>,
    /// The last thing said about it ("Copied 3 of 5 clips").
    pub latest: String,
    pub started: Instant,
    pub state: JobState,
}

/// Opus asking permission: read out, answered by voice or with Yes / No.
#[derive(Debug, Clone, PartialEq)]
pub struct VoiceApproval {
    pub id: String,
    /// The question as it is spoken ("Opus wants to delete 3 files in Downloads. Yes or no?").
    pub question: String,
    /// For a public action: exactly what will go out (a caption, a file, a link).
    pub content: Option<String>,
    /// A post, a message or a purchase: always asked.
    pub public: bool,
    /// No answer by then counts as no.
    pub deadline: Option<Instant>,
}

/// Recording the person's voice for the speaker profile.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Enrolment {
    #[default]
    Idle,
    Recording {
        seconds: f32,
        needed: f32,
    },
    Finished {
        ok: bool,
    },
}

/// Designing the assistant's voice.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum VoiceDesign {
    #[default]
    Idle,
    Designing,
    /// Designed and played; [`VoiceAction::UseDesignedVoice`] keeps it.
    Ready(AssistantVoice),
    Failed(String),
}

/// Everything the voice screens show, as of now.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct VoiceView {
    /// The voice service is running and connected.
    pub running: bool,
    /// Start was asked for and the service isn't ready yet.
    pub starting: bool,
    /// The microphone is open (waiting for the wake word, or always listening).
    pub listening: bool,
    /// Someone is talking and it is being written down.
    pub capturing: bool,
    /// The assistant is speaking.
    pub speaking: bool,
    /// The person's voice is enrolled.
    pub enrolled: bool,
    /// What is being said, written down as it comes (empty when nobody is talking).
    pub partial: String,
    /// The conversation, oldest first (the backend keeps the last few hundred).
    pub lines: Vec<VoiceLine>,
    pub jobs: Vec<OpusJob>,
    /// Questions waiting for the person's yes or no, oldest first.
    pub approvals: Vec<VoiceApproval>,
    pub enrolment: Enrolment,
    pub design: VoiceDesign,
    /// Audio devices, by name (empty until listed).
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    pub parts: VoiceParts,
    pub config: VoiceConfig,
    /// The last problem, in the person's words.
    pub error: Option<String>,
}

/// What the person asks of the voice parts.
#[derive(Debug, Clone, PartialEq)]
pub enum VoiceAction {
    /// Start the voice service (and connect to it).
    Start,
    /// Stop the voice service. Running Opus jobs go on.
    Stop,
    /// The push-to-talk key (or the microphone button) went down or up.
    PushToTalk { down: bool },
    /// Stop speaking now.
    StopSpeaking,
    /// Answer a question from [`VoiceView::approvals`].
    Answer { id: String, yes: bool },
    /// Stop an Opus job.
    CancelJob { id: String },
    /// Start recording the person's voice for the speaker profile.
    EnrolStart,
    /// Stop recording and save the profile (if there was enough).
    EnrolStop,
    /// Design an assistant voice from a description and play the sample with it.
    DesignVoice { description: String, sample_text: String },
    /// Keep the voice from [`VoiceDesign::Ready`] as the assistant's.
    UseDesignedVoice,
    /// Apply and keep new settings.
    SetConfig(VoiceConfig),
    /// List the audio devices again.
    RefreshDevices,
    /// Look at every part again.
    CheckStatus,
}

/// The voice parts, as the UI sees them. The integration implements this over the voice client
/// (`craft_anything_engines::voice`) and the brain and Opus executor (`craft_anything_ai::voice`).
///
/// Both methods run on the UI thread every frame, so neither may block or do I/O: events arrive
/// on the backend's own threads and are folded into the view; commands are queued to them.
pub trait VoiceBackend: Send {
    /// Fold in what has happened since the last call (without waiting) and return the current
    /// picture. A change made by [`VoiceBackend::act`] shows in the very next view, even before
    /// the service has confirmed it (a new [`VoiceConfig`], an answered question).
    fn view(&mut self) -> VoiceView;

    /// Do what the person asked. Never blocks; a failure shows later as [`VoiceView::error`].
    fn act(&mut self, action: VoiceAction);

    /// Called once with a function that redraws the window: call it when an event arrives, so
    /// the screens don't have to redraw on a timer to notice.
    fn set_waker(&mut self, _wake: Arc<dyn Fn() + Send + Sync>) {}
}

// ---- The app's side -------------------------------------------------------------------------

/// The microphone's state, as the title bar button shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicState {
    Off,
    Starting,
    /// Ready: waiting for the key (push to talk), the wake word or speech.
    Listening,
    /// The person is talking.
    Capturing,
    /// The assistant is talking.
    Speaking,
    /// Opus is doing something the person asked.
    Working,
}

/// The voice parts and what the screens keep between frames.
pub struct Voice {
    backend: Box<dyn VoiceBackend>,
    /// The view drawn this frame (refreshed in [`tick`] and after every action).
    pub view: VoiceView,
    /// The panel that slides in on the screens without the Workbench console.
    pub panel_open: bool,
    /// Settings → Voice's "Enrol my voice" card is showing.
    pub enrol_open: bool,
    /// Settings → Voice's "Design the assistant's voice" boxes.
    pub design_description: String,
    pub design_sample: String,
    /// The push-to-talk key (or the microphone button) is held down.
    ptt_held: bool,
    waker_set: bool,
}

impl Voice {
    pub fn new(backend: Box<dyn VoiceBackend>) -> Self {
        let mut backend = backend;
        let view = backend.view();
        Voice {
            backend,
            view,
            panel_open: false,
            enrol_open: false,
            design_description: String::new(),
            design_sample: String::new(),
            ptt_held: false,
            waker_set: false,
        }
    }

    /// Use other voice parts (the integration's, or a test's stand-in).
    pub fn set_backend(&mut self, backend: Box<dyn VoiceBackend>) {
        self.backend = backend;
        self.waker_set = false;
        self.ptt_held = false;
        self.refresh();
    }

    /// Ask the voice parts for something, and show the result at once.
    pub fn act(&mut self, action: VoiceAction) {
        self.backend.act(action);
        self.refresh();
    }

    fn refresh(&mut self) {
        self.view = self.backend.view();
    }

    /// Change one setting and apply it.
    pub fn configure(&mut self, change: impl FnOnce(&mut VoiceConfig)) {
        let mut config = self.view.config.clone();
        change(&mut config);
        if config != self.view.config {
            self.act(VoiceAction::SetConfig(config));
        }
    }

    pub fn set_task_folder(&mut self, folder: Option<PathBuf>) {
        self.configure(|c| c.task_folder = folder);
    }

    /// Whether the push-to-talk key is held (or the microphone button was pressed to talk).
    pub fn talking(&self) -> bool {
        self.ptt_held
    }

    fn ptt(&mut self, down: bool) {
        if self.ptt_held != down {
            self.ptt_held = down;
            self.act(VoiceAction::PushToTalk { down });
        }
    }

    pub fn mic_state(&self) -> MicState {
        mic_state(&self.view)
    }

    /// Opus jobs still running.
    pub fn running_jobs(&self) -> usize {
        self.view.jobs.iter().filter(|j| j.state == JobState::Running).count()
    }

    /// The push-to-talk key, when its name is one egui knows.
    pub fn ptt_key(&self) -> Option<egui::Key> {
        egui::Key::from_name(self.view.config.push_to_talk_key.trim())
    }
}

/// The microphone's state from what the voice parts say.
pub fn mic_state(v: &VoiceView) -> MicState {
    if v.starting {
        MicState::Starting
    } else if !v.running {
        MicState::Off
    } else if v.capturing {
        MicState::Capturing
    } else if v.speaking {
        MicState::Speaking
    } else if v.jobs.iter().any(|j| j.state == JobState::Running) {
        MicState::Working
    } else {
        MicState::Listening
    }
}

/// The word painted beside the microphone, and what it says to screen readers.
pub fn mic_word(state: MicState, mode: ListenMode) -> String {
    match state {
        MicState::Off => tr("Off"),
        MicState::Starting => tr("Starting…"),
        MicState::Listening if mode == ListenMode::PushToTalk => tr("Ready"),
        MicState::Listening => tr("Listening"),
        MicState::Capturing => tr("Hearing you"),
        MicState::Speaking => tr("Speaking"),
        MicState::Working => tr("Opus working"),
    }
}

fn mic_colour(t: &Tokens, state: MicState) -> Color32 {
    match state {
        MicState::Off | MicState::Starting => t.icon,
        MicState::Listening => t.success,
        MicState::Capturing => t.danger,
        MicState::Speaking => t.accent,
        MicState::Working => t.warning,
    }
}

/// What to do to talk, for the hint under the microphone.
fn how_to_talk(v: &VoiceView) -> String {
    match v.config.listen_mode {
        ListenMode::PushToTalk => trf("Hold {key} (or press the microphone) and talk.", &[("key", &v.config.push_to_talk_key)]),
        ListenMode::WakeWord => trf("Say “{word}”, then talk.", &[("word", &v.config.wake_word)]),
        ListenMode::Always => tr("Just talk: a pause ends what you say."),
    }
}

/// Per frame: the latest view, the push-to-talk key, and the next redraw.
pub fn tick(app: &mut CraftApp, ctx: &egui::Context) {
    let voice = &mut app.voice;
    if !voice.waker_set {
        let c = ctx.clone();
        voice.backend.set_waker(Arc::new(move || c.request_repaint()));
        voice.waker_set = true;
    }
    voice.refresh();
    // The push-to-talk key while the window has the keyboard. Repeats while held are ignored,
    // and leaving the window lets go of it (its key-up would go elsewhere).
    // Taken in order: a quick tap can put the key's down and up in the same frame.
    let key = voice.ptt_key();
    let presses: Vec<bool> = ctx.input(|i| {
        i.events
            .iter()
            .filter_map(|e| match e {
                egui::Event::Key { key: k, pressed, repeat: false, .. } if Some(*k) == key => Some(*pressed),
                egui::Event::WindowFocused(false) => Some(false),
                _ => None,
            })
            .collect()
    });
    for down in presses {
        if down && !app.voice.view.running {
            app.toast(tr("Voice is off. Start it with the microphone in the title bar."), false);
        } else {
            app.voice.ptt(down);
        }
    }
    // Events redraw through the waker; only the clocks and the pulse need a timer.
    let v = &app.voice.view;
    if v.capturing {
        ctx.request_repaint_after(widgets::FRAME * 3);
    } else if v.starting || !v.approvals.is_empty() || v.jobs.iter().any(|j| j.state == JobState::Running) {
        ctx.request_repaint_after(Duration::from_secs(1));
    }
}

/// The microphone button was pressed: start the voice, or talk, or stop the assistant speaking.
fn mic_clicked(app: &mut CraftApp) {
    match app.voice.mic_state() {
        MicState::Off => {
            app.voice.act(VoiceAction::Start);
            show_conversation(app);
        }
        MicState::Starting => show_conversation(app),
        MicState::Speaking => app.voice.act(VoiceAction::StopSpeaking),
        MicState::Capturing => app.voice.ptt(false),
        MicState::Listening | MicState::Working => app.voice.ptt(true),
    }
}

/// Show the conversation: the console's Voice tab on the Workbench, the panel elsewhere.
pub fn show_conversation(app: &mut CraftApp) {
    if !app.beginner() && app.state.screen == crate::state::Screen::Workbench {
        app.state.bottom = crate::state::BottomTab::Voice;
        app.state.bottom_open = true;
    } else {
        app.voice.panel_open = true;
    }
}

/// The microphone in the title bar: an icon when voice is off; an icon and the state's word when
/// it's on, in the state's colour (red while the person talks). `s` is the bar's scale.
pub fn mic_button(app: &mut CraftApp, ui: &mut Ui, s: f32, height: f32) {
    let t = Tokens::get(ui.ctx());
    let state = app.voice.mic_state();
    let word = mic_word(state, app.voice.view.config.listen_mode);
    let col = mic_colour(&t, state);
    let icon = match state {
        MicState::Off => "mic-off",
        MicState::Speaking => "volume-2",
        _ => "mic",
    };
    let label = (state != MicState::Off).then(|| widgets::label_galley(ui, &word, (11.0 + 1.5) * s - 1.5, col));
    let w = label.as_ref().map_or(height, |g| g.size().x + 34.0 * s);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, height), egui::Sense::click());
    let h = widgets::hover_t(ui, &resp);
    let fill = if state == MicState::Off { theme::mix(Color32::TRANSPARENT, t.hover, h) } else { theme::mix(t.raised, t.hover, h) };
    let edge = if state == MicState::Off { Color32::TRANSPARENT } else { col.gamma_multiply(0.5) };
    ui.painter().rect(rect, t.radius, fill, egui::Stroke::new(s, edge), egui::StrokeKind::Inside);
    let icon_at = if label.is_some() { egui::pos2(rect.left() + 15.0 * s, rect.center().y) } else { rect.center() };
    if state == MicState::Capturing {
        // A soft pulse behind the microphone while it hears the person.
        let now = ui.input(|i| i.time);
        let pulse = (now * 4.0).sin() as f32 * 0.5 + 0.5;
        ui.painter().circle_filled(icon_at, (8.0 + 3.0 * pulse) * s, col.gamma_multiply(0.18 + 0.12 * pulse));
        widgets::animate(ui, widgets::FRAME);
    }
    icons::paint(ui, egui::Rect::from_center_size(icon_at, Vec2::splat(16.0 * s)), icon, 15.0 * s, col);
    if let Some(g) = label {
        ui.painter().galley_with_override_text_color(egui::pos2(rect.left() + 28.0 * s, rect.center().y - g.size().y / 2.0), g, col);
    }
    widgets::focus_ring(ui, &resp, rect);
    let name = trf("Voice: {state}", &[("state", &word)]);
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, name.clone()));
    let tip = match state {
        MicState::Off => tr("Talk to Craft Anything. Click to start voice."),
        MicState::Starting => tr("Voice is starting."),
        MicState::Capturing => tr("Hearing you. Click (or let go of the key) when you've finished."),
        MicState::Speaking => tr("Click to stop it speaking."),
        MicState::Listening | MicState::Working => how_to_talk(&app.voice.view),
    };
    let resp = resp.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand);
    if resp.clicked() {
        mic_clicked(app);
    }
    // While voice is on, the conversation is one click away.
    if state != MicState::Off && icons::button(ui, "message-square", height, false, "Voice conversation").clicked() {
        show_conversation(app);
    }
}

/// The panel that slides in from the right on screens without the Workbench console.
pub fn side_panel(app: &mut CraftApp, ui: &mut Ui) {
    let t = Tokens::get(ui.ctx());
    let mut open = app.voice.panel_open;
    egui::Panel::right("voice-panel")
        .exact_size(360.0)
        .resizable(false)
        .frame(egui::Frame::new().fill(t.chrome).inner_margin(egui::Margin::symmetric(14, 12)).stroke(egui::Stroke::new(1.0, t.separator)))
        .show_collapsible(ui, &mut open, |ui| {
            crate::guard_ui(ui, "voice panel", |ui| {
                ui.horizontal(|ui| {
                    icons::show(ui, "mic", 18.0, t.accent);
                    ui.label(theme::heading(tr("Voice"), theme::CARD));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if icons::button(ui, "x", 28.0, false, "Close the voice panel").clicked() {
                            app.voice.panel_open = false;
                        }
                    });
                });
                ui.add_space(8.0);
                conversation(app, ui);
            });
        });
    app.voice.panel_open &= open;
}

/// The conversation: the state and how to talk, questions waiting for an answer, Opus's jobs,
/// what was said and what is being said. The console's Voice tab and the side panel draw this.
pub fn conversation(app: &mut CraftApp, ui: &mut Ui) {
    let t = Tokens::get(ui.ctx());
    let mut actions: Vec<VoiceAction> = Vec::new();
    let mut settings = false;
    let v = &app.voice.view;
    let state = mic_state(v);
    ui.horizontal_wrapped(|ui| {
        let col = mic_colour(&t, state);
        let (dot, _) = ui.allocate_exact_size(Vec2::splat(10.0), egui::Sense::hover());
        ui.painter().circle_filled(dot.center(), 4.0, col);
        ui.label(RichText::new(mic_word(state, v.config.listen_mode)).font(theme::medium(theme::SMALL)).color(t.text));
        if v.running {
            ui.label(RichText::new(how_to_talk(v)).color(t.text_faint).size(theme::CAPTION));
        }
    });
    ui.horizontal(|ui| {
        if !v.running && !v.starting {
            if widgets::primary_icon(ui, Some("mic"), "Start voice", true).clicked() {
                actions.push(VoiceAction::Start);
            }
        } else if v.running {
            if v.speaking && widgets::secondary(ui, Some("circle-stop"), "Stop speaking").clicked() {
                actions.push(VoiceAction::StopSpeaking);
            }
            if widgets::secondary(ui, Some("mic-off"), "Stop voice").clicked() {
                actions.push(VoiceAction::Stop);
            }
        }
        if icons::button(ui, "settings", 28.0, false, "Voice settings").clicked() {
            settings = true;
        }
    });
    if let Some(e) = &v.error {
        ui.add(egui::Label::new(RichText::new(e).color(t.danger).size(theme::CAPTION)).wrap());
    }
    if v.running && !v.enrolled {
        ui.add(
            egui::Label::new(
                RichText::new(tr("Your voice isn't enrolled yet, so anyone's voice could give commands. Enrol it in Settings → Voice."))
                    .color(t.warning)
                    .size(theme::CAPTION),
            )
            .wrap(),
        );
    }
    ui.add_space(6.0);
    // Questions and jobs beside the conversation where there's room (the console), above it
    // in the narrow panel.
    let pending = !v.approvals.is_empty() || !v.jobs.is_empty();
    if pending && ui.available_width() >= 720.0 {
        ui.columns(2, |cols| {
            if let [left, right] = cols {
                egui::ScrollArea::vertical().id_salt("voice-pending").auto_shrink([false, false]).show(left, |ui| pending_work(ui, &t, v, &mut actions));
                lines(right, &t, v);
            }
        });
    } else {
        pending_work(ui, &t, v, &mut actions);
        lines(ui, &t, v);
    }
    for a in actions {
        app.voice.act(a);
    }
    if settings {
        app.open_settings(crate::state::SettingsTab::Voice);
    }
}

/// The questions waiting for an answer, then Opus's jobs.
fn pending_work(ui: &mut Ui, t: &Tokens, v: &VoiceView, actions: &mut Vec<VoiceAction>) {
    for a in &v.approvals {
        approval_card(ui, t, a, actions);
        ui.add_space(6.0);
    }
    if !v.jobs.is_empty() {
        widgets::caption(ui, "Opus is working on…");
        for j in &v.jobs {
            job_row(ui, t, j, actions);
        }
        ui.add_space(6.0);
    }
}

/// What was said, and what is being said, newest at the bottom.
fn lines(ui: &mut Ui, t: &Tokens, v: &VoiceView) {
    egui::ScrollArea::vertical().id_salt("voice-lines").auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
        if v.lines.is_empty() && v.partial.is_empty() {
            ui.label(RichText::new(tr("What you say and what Craft Anything answers shows here.")).color(t.text_faint).size(theme::CAPTION));
        }
        for line in &v.lines {
            line_row(ui, t, line);
        }
        if v.capturing || !v.partial.is_empty() {
            ui.horizontal_wrapped(|ui| {
                let (dot, _) = ui.allocate_exact_size(Vec2::new(10.0, 14.0), egui::Sense::hover());
                ui.painter().circle_filled(dot.center(), 3.5, t.danger);
                let text = if v.partial.is_empty() { tr("Listening…") } else { format!("{}…", v.partial) };
                ui.add(egui::Label::new(RichText::new(text).italics().color(t.text_dim)).wrap());
            });
        }
    });
}

fn approval_card(ui: &mut Ui, t: &Tokens, a: &VoiceApproval, actions: &mut Vec<VoiceAction>) {
    egui::Frame::new()
        .fill(t.card)
        .stroke(egui::Stroke::new(1.0, t.warning.gamma_multiply(0.6)))
        .corner_radius(t.r())
        .inner_margin(egui::Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                icons::show(ui, "triangle-alert", 15.0, t.warning);
                let title = if a.public { tr("Opus asks before anything public") } else { tr("Opus asks") };
                ui.label(RichText::new(title).font(theme::semibold(theme::SMALL)).color(t.text));
            });
            ui.add(egui::Label::new(RichText::new(&a.question).color(t.text)).wrap());
            if let Some(c) = &a.content {
                ui.label(RichText::new(tr("What goes out:")).color(t.text_dim).size(theme::CAPTION));
                egui::Frame::new().fill(t.code_bg).corner_radius(t.r_sm()).inner_margin(egui::Margin::same(6)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.add(egui::Label::new(RichText::new(c).font(theme::mono(12.0)).color(t.text)).wrap());
                });
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if widgets::primary_icon(ui, Some("check"), "Yes", true).clicked() {
                    actions.push(VoiceAction::Answer { id: a.id.clone(), yes: true });
                }
                if widgets::secondary(ui, Some("x"), "No").clicked() {
                    actions.push(VoiceAction::Answer { id: a.id.clone(), yes: false });
                }
                if let Some(d) = a.deadline {
                    let left = d.saturating_duration_since(Instant::now());
                    let text = trf("No answer in {time} counts as no.", &[("time", &crate::usage::duration_words(left))]);
                    ui.add(egui::Label::new(RichText::new(text).color(t.text_faint).size(theme::CAPTION)).wrap());
                }
            });
        });
}

fn job_row(ui: &mut Ui, t: &Tokens, j: &OpusJob, actions: &mut Vec<VoiceAction>) {
    ui.horizontal(|ui| {
        match j.state {
            JobState::Running => {
                ui.add(widgets::Spinner::new().size(14.0).color(t.accent));
            }
            JobState::Done => {
                icons::show(ui, "check", 14.0, t.success);
            }
            JobState::Failed => {
                icons::show(ui, "triangle-alert", 14.0, t.danger);
            }
            JobState::Stopped => {
                icons::show(ui, "circle-stop", 14.0, t.text_faint);
            }
        }
        ui.add(egui::Label::new(RichText::new(&j.task).font(theme::medium(theme::SMALL)).color(t.text)).truncate());
    });
    ui.horizontal(|ui| {
        ui.add_space(22.0);
        match j.state {
            JobState::Running => {
                let bar = egui::ProgressBar::new(j.progress.unwrap_or(0.0).clamp(0.0, 1.0)).desired_width(120.0).desired_height(6.0).fill(t.accent);
                ui.add(if j.progress.is_none() { bar.animate(true) } else { bar });
                let secs = j.started.elapsed();
                ui.label(RichText::new(crate::usage::duration_words(secs)).font(theme::mono(12.0)).color(t.text_faint));
                if widgets::secondary(ui, Some("circle-stop"), "Stop").clicked() {
                    actions.push(VoiceAction::CancelJob { id: j.id.clone() });
                }
            }
            JobState::Done => {
                ui.label(RichText::new(tr("Done")).color(t.success).size(theme::CAPTION));
            }
            JobState::Failed => {
                ui.label(RichText::new(tr("Didn't work")).color(t.danger).size(theme::CAPTION));
            }
            JobState::Stopped => {
                ui.label(RichText::new(tr("Stopped")).color(t.text_faint).size(theme::CAPTION));
            }
        }
    });
    if !j.latest.is_empty() {
        ui.horizontal(|ui| {
            ui.add_space(22.0);
            ui.add(egui::Label::new(RichText::new(&j.latest).color(t.text_dim).size(theme::CAPTION)).wrap());
        });
    }
    ui.add_space(4.0);
}

/// A name in a column of its own width, left-aligned (cut short when it doesn't fit).
fn name_cell(ui: &mut Ui, width: f32, text: RichText) {
    ui.allocate_ui_with_layout(Vec2::new(width, 16.0), egui::Layout::left_to_right(egui::Align::Min), |ui| {
        ui.set_width(width);
        ui.add(egui::Label::new(text).truncate());
    });
}

fn line_row(ui: &mut Ui, t: &Tokens, line: &VoiceLine) {
    let (who, who_col, text_col) = match line.speaker {
        Speaker::You => (tr("You"), t.accent_ink, t.text),
        Speaker::Assistant => (tr("Craft Anything"), t.text_dim, t.text),
        Speaker::NotYou => (tr("Not your voice"), t.text_faint, t.text_faint),
    };
    ui.horizontal_top(|ui| {
        name_cell(ui, 96.0, RichText::new(who).font(theme::medium(theme::CAPTION)).color(who_col));
        let mut text = RichText::new(&line.text).color(text_col);
        if line.speaker == Speaker::NotYou {
            text = text.italics();
        }
        let r = ui.add(egui::Label::new(text).wrap());
        if line.speaker == Speaker::NotYou {
            r.on_hover_text(tr("It didn't sound like your enrolled voice, so it was ignored and not sent to Opus."));
        }
    });
    ui.add_space(3.0);
}

// ---- Settings → Voice -------------------------------------------------------------------------

/// The four sentences read aloud to enrol (about 20 seconds in all).
pub fn enrol_sentences() -> [String; 4] {
    [
        tr("Craft Anything, please open my newest project and show me what changed."),
        tr("I'd like a small game where a red ball bounces between two walls."),
        tr("Tomorrow morning I will finish the trailer and check the music."),
        tr("Seven green bottles stood in a row on the kitchen window."),
    ]
}

/// Which sentence to read now, from how much has been recorded.
pub fn enrol_sentence_index(seconds: f32, needed: f32) -> usize {
    if !(seconds.is_finite() && needed.is_finite()) || needed <= 0.0 {
        return 0;
    }
    ((seconds / needed * 4.0).floor().max(0.0) as usize).min(3)
}

fn part_word(state: PartState) -> String {
    match state {
        PartState::Ready => tr("Ready"),
        PartState::Off => tr("Off"),
        PartState::NotRunning => tr("Not running"),
        PartState::MissingModel => tr("Missing model"),
        PartState::NotSetUp => tr("Not set up"),
    }
}

/// One part's status row: a mark, its name, the state's word and what's what.
fn part_row(ui: &mut Ui, name: &str, part: &PartStatus) {
    let t = Tokens::get(ui.ctx());
    let (icon, col) = match part.state {
        PartState::Ready => ("check", t.success),
        PartState::Off => ("minus", t.text_faint),
        _ => ("triangle-alert", t.warning),
    };
    ui.horizontal(|ui| {
        icons::show(ui, icon, 14.0, col);
        name_cell(ui, 140.0, RichText::new(name).font(theme::medium(theme::CAPTION)));
        ui.label(RichText::new(part_word(part.state)).color(col).size(theme::CAPTION));
        if !part.detail.is_empty() {
            ui.add(egui::Label::new(RichText::new(&part.detail).color(t.text_faint).size(theme::CAPTION)).truncate());
        }
    });
}

fn settings_row(ui: &mut Ui, title: &str, sub: &str, add: impl FnOnce(&mut Ui)) {
    let t = Tokens::get(ui.ctx());
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.set_width(200.0);
            ui.label(RichText::new(title).font(theme::medium(theme::SMALL)));
            if !sub.is_empty() {
                ui.add(egui::Label::new(RichText::new(sub).color(t.text_faint).size(theme::CAPTION)).wrap());
            }
        });
        add(ui);
    });
    ui.add_space(10.0);
}

fn device_picker(ui: &mut Ui, id: &str, name: &str, current: &Option<String>, devices: &[String]) -> Option<Option<String>> {
    let mut picked = None;
    let shown = current.clone().unwrap_or_else(|| tr("System default"));
    let r = egui::ComboBox::from_id_salt(id).icon(widgets::combo_icon).width(220.0).selected_text(shown).show_ui(ui, |ui| {
        if ui.selectable_label(current.is_none(), tr("System default")).clicked() {
            picked = Some(None);
        }
        for d in devices {
            if ui.selectable_label(current.as_ref() == Some(d), d).clicked() {
                picked = Some(Some(d.clone()));
            }
        }
    });
    widgets::named(r.response, name);
    picked
}

/// Settings → Voice. `path_field` is Settings' own path box.
pub fn settings(app: &mut CraftApp, ui: &mut Ui, path_field: fn(&mut Ui, &mut Option<PathBuf>, &str, &str) -> bool) {
    let t = Tokens::get(ui.ctx());
    widgets::caption(ui, "Voice");
    ui.add(
        egui::Label::new(
            RichText::new(tr(
                "Talk to Craft Anything and hear it answer. Listening, understanding and speaking happen on this computer; what you ask to be done is done by Opus in Claude Code.",
            ))
            .color(t.text_faint)
            .size(theme::CAPTION),
        )
        .wrap(),
    );
    ui.add_space(8.0);
    let mut actions: Vec<VoiceAction> = Vec::new();
    let v = app.voice.view.clone();
    // The parts and Start / Stop.
    part_row(ui, &tr("Voice service"), &v.parts.service);
    part_row(ui, &tr("Speech recognition"), &v.parts.speech_recognition);
    part_row(ui, &tr("Speech"), &v.parts.speech);
    part_row(ui, &tr("Speaker profile"), &v.parts.speaker_profile);
    part_row(ui, &tr("Wake word"), &v.parts.wake_word);
    part_row(ui, "Opus", &v.parts.opus);
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if v.running {
            if widgets::secondary(ui, Some("mic-off"), "Stop voice").clicked() {
                actions.push(VoiceAction::Stop);
            }
        } else if !v.starting && widgets::primary_icon(ui, Some("mic"), "Start voice", true).clicked() {
            actions.push(VoiceAction::Start);
        }
        if widgets::secondary(ui, Some("refresh-cw"), "Check").clicked() {
            actions.push(VoiceAction::CheckStatus);
        }
        if v.starting {
            ui.add(widgets::Spinner::new().size(14.0).color(t.accent));
        }
    });
    if let Some(e) = &v.error {
        ui.add(egui::Label::new(RichText::new(e).color(t.danger).size(theme::CAPTION)).wrap());
    }
    ui.add_space(12.0);

    // How it listens.
    let mut config = v.config.clone();
    settings_row(
        ui,
        &tr("Listen"),
        &tr("Push to talk listens only while you hold the key. The wake word and always listening keep the microphone open."),
        |ui| {
            let sel = ListenMode::ALL.iter().position(|m| *m == config.listen_mode).unwrap_or(0);
            if let Some(i) = widgets::segmented(ui, &["Push to talk", "After the wake word", "Always listening"], sel, 30.0) {
                config.listen_mode = ListenMode::ALL.get(i).copied().unwrap_or_default();
            }
        },
    );
    settings_row(ui, &tr("Push-to-talk key"), &tr("Works while Craft Anything is the window in front."), |ui| {
        egui::ComboBox::from_id_salt("voice-ptt-key").icon(widgets::combo_icon).selected_text(config.push_to_talk_key.clone()).show_ui(ui, |ui| {
            for n in 1..=12 {
                let name = format!("F{n}");
                if ui.selectable_label(config.push_to_talk_key == name, &name).clicked() {
                    config.push_to_talk_key = name;
                }
            }
        });
    });
    settings_row(ui, &tr("What to say first"), &tr("The wake word, said before a request when listening for it."), |ui| {
        let r = ui.add(
            egui::TextEdit::singleline(&mut config.wake_word)
                .desired_width(220.0)
                .font(theme::mono(12.0))
                .frame(widgets::text_field_frame_compact(&t))
                .interactive(config.listen_mode == ListenMode::WakeWord),
        );
        widgets::named(r, "The wake word");
    });
    settings_row(ui, &tr("Microphone"), "", |ui| {
        if let Some(d) = device_picker(ui, "voice-input", "Microphone", &config.input_device, &v.inputs) {
            config.input_device = d;
        }
        if icons::button(ui, "refresh-cw", 26.0, false, "List the devices again").clicked() {
            actions.push(VoiceAction::RefreshDevices);
        }
    });
    settings_row(ui, &tr("Speakers"), "", |ui| {
        if let Some(d) = device_picker(ui, "voice-output", "Speakers", &config.output_device, &v.outputs) {
            config.output_device = d;
        }
    });
    settings_row(
        ui,
        &tr("Only my voice"),
        &tr("How closely a voice must match yours to give commands. Higher ignores more of other people, and sometimes you."),
        |ui| {
            let r = ui.add(egui::Slider::new(&mut config.speaker_threshold, 0.3..=0.9).step_by(0.01).fixed_decimals(2));
            widgets::named(r, "Only my voice");
        },
    );
    settings_row(ui, &tr("Opus works in"), &tr("The folder Opus starts in for what you ask by voice. Empty: your home folder."), |ui| {
        if path_field(ui, &mut config.task_folder, "Opus works in", "your home folder") {
            // Applied below with the rest.
        }
        if icons::button(ui, "folder-open", 26.0, false, "Choose…").clicked() {
            app.pick(crate::Pick::VoiceTaskFolder);
        }
    });
    settings_row(ui, &tr("Start with the app"), "", |ui| {
        widgets::toggle(ui, &mut config.auto_start, "Start voice when Craft Anything opens");
    });
    ui.horizontal(|ui| {
        icons::show(ui, "lock", 14.0, t.text_faint);
        ui.add(
            egui::Label::new(
                RichText::new(tr("Anything public (a post, a message, a purchase) is always asked first, and what goes out is read to you."))
                    .color(t.text_faint)
                    .size(theme::CAPTION),
            )
            .wrap(),
        );
    });
    ui.horizontal(|ui| {
        icons::show(ui, "bot", 14.0, t.text_faint);
        ui.add(
            egui::Label::new(
                RichText::new(tr("The local model only listens and talks. Everything you ask to be done is done by Opus in Claude Code."))
                    .color(t.text_faint)
                    .size(theme::CAPTION),
            )
            .wrap(),
        );
    });
    if config != v.config {
        actions.push(VoiceAction::SetConfig(config));
    }
    ui.add_space(14.0);
    enrol_section(app, ui, &v, &mut actions);
    ui.add_space(14.0);
    design_section(app, ui, &v, &mut actions);
    for a in actions {
        app.voice.act(a);
    }
}

fn enrol_section(app: &mut CraftApp, ui: &mut Ui, v: &VoiceView, actions: &mut Vec<VoiceAction>) {
    let t = Tokens::get(ui.ctx());
    widgets::caption(ui, "Your voice");
    let status = if v.enrolled { tr("Your voice is enrolled: only you can give commands.") } else { tr("Not enrolled yet: any voice can give commands.") };
    ui.label(RichText::new(status).color(if v.enrolled { t.success } else { t.warning }).size(theme::CAPTION));
    ui.add_space(4.0);
    let recording = matches!(v.enrolment, Enrolment::Recording { .. });
    if !app.voice.enrol_open && !recording {
        let label = if v.enrolled { tr("Enrol my voice again") } else { tr("Enrol my voice") };
        if widgets::secondary(ui, Some("fingerprint"), &label).clicked() {
            app.voice.enrol_open = true;
        }
        return;
    }
    widgets::card(&t).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new(tr("Enrol my voice")).font(theme::semibold(theme::SMALL)));
        ui.add(
            egui::Label::new(
                RichText::new(tr("Press Start recording, then read these four sentences aloud at your normal pace. About 20 seconds in all."))
                    .color(t.text_dim)
                    .size(theme::CAPTION),
            )
            .wrap(),
        );
        ui.add_space(6.0);
        let current = match v.enrolment {
            Enrolment::Recording { seconds, needed } => Some(enrol_sentence_index(seconds, needed)),
            _ => None,
        };
        for (i, s) in enrol_sentences().iter().enumerate() {
            let on = current == Some(i);
            let col = if on { t.text } else { t.text_dim };
            let mut text = RichText::new(format!("{}. {s}", i + 1)).color(col);
            if on {
                text = text.strong();
            }
            ui.add(egui::Label::new(text).wrap());
        }
        ui.add_space(6.0);
        match v.enrolment {
            Enrolment::Recording { seconds, needed } => {
                let frac = if needed > 0.0 { (seconds / needed).clamp(0.0, 1.0) } else { 0.0 };
                ui.add(egui::ProgressBar::new(frac).desired_width(240.0).desired_height(8.0).fill(t.accent));
                let said = trf("{done} of {needed} seconds", &[("done", &format!("{:.0}", seconds.max(0.0))), ("needed", &format!("{:.0}", needed.max(0.0)))]);
                ui.label(RichText::new(said).color(t.text_dim).size(theme::CAPTION));
                let stop = if seconds >= needed {
                    widgets::primary_icon(ui, Some("check"), "Finish", true)
                } else {
                    widgets::secondary(ui, Some("circle-stop"), "Stop")
                };
                if stop.clicked() {
                    actions.push(VoiceAction::EnrolStop);
                }
            }
            _ => {
                match v.enrolment {
                    Enrolment::Finished { ok: true } => {
                        ui.label(RichText::new(tr("Done. Your voice is enrolled.")).color(t.success).size(theme::CAPTION));
                    }
                    Enrolment::Finished { ok: false } => {
                        ui.label(RichText::new(tr("That wasn't enough of your voice. Try again and read all four.")).color(t.warning).size(theme::CAPTION));
                    }
                    _ => {}
                }
                ui.horizontal(|ui| {
                    if v.running {
                        if widgets::primary_icon(ui, Some("mic"), "Start recording", true).clicked() {
                            actions.push(VoiceAction::EnrolStart);
                        }
                    } else {
                        widgets::primary_icon(ui, Some("mic"), "Start recording", false);
                        ui.label(RichText::new(tr("Start voice first.")).color(t.text_faint).size(theme::CAPTION));
                    }
                    if widgets::secondary(ui, None, "Close").clicked() {
                        app.voice.enrol_open = false;
                    }
                });
            }
        }
    });
}

fn design_section(app: &mut CraftApp, ui: &mut Ui, v: &VoiceView, actions: &mut Vec<VoiceAction>) {
    let t = Tokens::get(ui.ctx());
    widgets::caption(ui, "The assistant's voice");
    let now = if v.config.assistant_voice.is_some() { tr("It speaks with the voice you designed.") } else { tr("It speaks with the built-in voice.") };
    ui.label(RichText::new(now).color(t.text_dim).size(theme::CAPTION));
    ui.add_space(4.0);
    ui.label(RichText::new(tr("Design the assistant's voice")).font(theme::medium(theme::SMALL)));
    let r = ui.add(
        egui::TextEdit::multiline(&mut app.voice.design_description)
            .hint_text(tr("Describe it: a calm, deep voice that speaks slowly…"))
            .desired_rows(2)
            .desired_width(f32::INFINITY)
            .frame(widgets::text_field_frame_compact(&t)),
    );
    widgets::named(r, "Describe the voice");
    if app.voice.design_sample.is_empty() {
        app.voice.design_sample = tr("Hello, I'm Craft Anything. What shall we make today?");
    }
    let r = ui.add(egui::TextEdit::singleline(&mut app.voice.design_sample).desired_width(f32::INFINITY).frame(widgets::text_field_frame_compact(&t)));
    widgets::named(r, "Sample sentence");
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        let ready = v.running && !app.voice.design_description.trim().is_empty() && v.design != VoiceDesign::Designing;
        if widgets::primary_icon(ui, Some("play"), "Preview", ready).clicked() && ready {
            actions.push(VoiceAction::DesignVoice {
                description: app.voice.design_description.trim().to_string(),
                sample_text: app.voice.design_sample.trim().to_string(),
            });
        }
        if matches!(v.design, VoiceDesign::Ready(_)) && widgets::secondary(ui, Some("check"), "Use this voice").clicked() {
            actions.push(VoiceAction::UseDesignedVoice);
        }
        match &v.design {
            VoiceDesign::Designing => {
                ui.add(widgets::Spinner::new().size(14.0).color(t.accent));
                ui.label(RichText::new(tr("Designing the voice…")).color(t.text_dim).size(theme::CAPTION));
            }
            VoiceDesign::Ready(_) => {
                ui.label(RichText::new(tr("Played. Keep it, or change the words and preview again.")).color(t.text_dim).size(theme::CAPTION));
            }
            VoiceDesign::Failed(e) => {
                ui.add(egui::Label::new(RichText::new(e).color(t.danger).size(theme::CAPTION)).wrap());
            }
            VoiceDesign::Idle if !v.running => {
                ui.label(RichText::new(tr("Start voice first.")).color(t.text_faint).size(theme::CAPTION));
            }
            VoiceDesign::Idle => {}
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mic_state_follows_the_view() {
        let mut v = VoiceView::default();
        assert_eq!(mic_state(&v), MicState::Off);
        v.starting = true;
        assert_eq!(mic_state(&v), MicState::Starting);
        v.starting = false;
        v.running = true;
        assert_eq!(mic_state(&v), MicState::Listening);
        v.jobs.push(OpusJob { id: "1".into(), task: "x".into(), progress: None, latest: String::new(), started: Instant::now(), state: JobState::Running });
        assert_eq!(mic_state(&v), MicState::Working);
        v.speaking = true;
        assert_eq!(mic_state(&v), MicState::Speaking);
        v.capturing = true;
        assert_eq!(mic_state(&v), MicState::Capturing);
    }

    #[test]
    fn enrol_sentence_index_stays_in_range() {
        assert_eq!(enrol_sentence_index(0.0, 20.0), 0);
        assert_eq!(enrol_sentence_index(5.0, 20.0), 1);
        assert_eq!(enrol_sentence_index(19.9, 20.0), 3);
        assert_eq!(enrol_sentence_index(400.0, 20.0), 3);
        assert_eq!(enrol_sentence_index(-3.0, 20.0), 0);
        assert_eq!(enrol_sentence_index(f32::NAN, 20.0), 0);
        assert_eq!(enrol_sentence_index(5.0, 0.0), 0);
    }

    #[test]
    fn listen_modes_round_trip_their_ids() {
        for m in ListenMode::ALL {
            assert_eq!(ListenMode::from_id(m.id()), Some(m));
        }
        assert_eq!(ListenMode::from_id("shout"), None);
    }
}
