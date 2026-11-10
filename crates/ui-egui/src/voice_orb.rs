//! The voice orb: a small floating sign at the bottom of every screen while voice is on, so it's
//! always clear what the voice is doing. Breathing while it listens, a flash when it starts
//! hearing, the microphone's real loudness as bars while it hears, turning dots while the local
//! model thinks, an amber pulse while Opus works, the speakers' loudness while it speaks, a
//! question card with Yes and No when something waits for an answer, a grey shake for someone
//! else's voice, and red with the message when something went wrong.
//!
//! It's painted (circles, arcs, bars), and asks for redraws only while something moves. The text
//! beside it is painted from the same string screen readers are given.

use crate::i18n::{tr, trf};
use crate::theme::{self, Tokens};
use crate::voice_ui::{JobState, Voice, VoiceAction, VoiceView};
use crate::{CraftApp, widgets};
use egui::{Color32, Pos2, Ui, Vec2};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Loudness samples kept for the bars (one per frame).
const HISTORY: usize = 24;
/// How long a flash, a shake, an error and a notice show.
const FLASH: Duration = Duration::from_millis(500);
const SHAKE: Duration = Duration::from_millis(700);
const ERROR: Duration = Duration::from_secs(8);
const NOTICE: Duration = Duration::from_secs(5);
/// The orb's radius.
const R: f32 = 17.0;
/// Bars painted while hearing or speaking.
const BARS: usize = 7;

/// What the orb animates from, kept between frames.
#[derive(Debug, Default)]
pub struct OrbAnim {
    pub flash_at: Option<Instant>,
    pub shake_at: Option<Instant>,
    pub error_at: Option<Instant>,
    /// Lines said above the orb for a few seconds ("Starting voice…", "Voice is on — …").
    pub notices: Vec<(String, Instant)>,
    /// The microphone's and the speakers' recent loudness, newest last.
    pub level_in: VecDeque<f32>,
    pub level_out: VecDeque<f32>,
}

impl OrbAnim {
    pub fn notice(&mut self, text: String) {
        let now = Instant::now();
        self.notices.retain(|(t, at)| *t != text && now.duration_since(*at) < NOTICE);
        self.notices.push((text, now));
    }

    /// The notices showing now, newest last (two at most).
    fn notices_now(&self, now: Instant) -> Vec<String> {
        let live: Vec<String> = self.notices.iter().filter(|(_, at)| now.duration_since(*at) < NOTICE).map(|(t, _)| t.clone()).collect();
        live[live.len().saturating_sub(2)..].to_vec()
    }
}

/// What the orb shows, most pressing first.
#[derive(Debug, Clone, PartialEq)]
pub enum OrbMode {
    /// Starting: step `step` of `steps` while the models load.
    Starting {
        step: u32,
        steps: u32,
    },
    Error(String),
    /// Something waits for a yes or no (the card above the orb has it).
    Asking,
    /// Someone else's voice was heard (and ignored).
    NotYou,
    Hearing,
    Speaking,
    Thinking,
    /// Opus works on `task` for `seconds`.
    Working {
        task: String,
        seconds: u64,
    },
    /// Hearing just started.
    Flash,
    Listening,
}

/// What the orb shows now, or nothing while voice is off.
pub fn mode(v: &VoiceView, anim: &OrbAnim, now: Instant) -> Option<OrbMode> {
    let recent = |at: Option<Instant>, d: Duration| at.is_some_and(|a| now.duration_since(a) < d);
    if !v.running && !v.starting {
        return None;
    }
    if let Some(e) = v.error.as_ref().filter(|_| recent(anim.error_at, ERROR)) {
        return Some(OrbMode::Error(e.clone()));
    }
    if v.starting || !v.running {
        let (step, steps) = v.loading.as_ref().map_or((0, 0), |(s, n, _)| (*s, *n));
        return Some(OrbMode::Starting { step, steps });
    }
    if !v.ready {
        let (step, steps) = v.loading.as_ref().map_or((0, 0), |(s, n, _)| (*s, *n));
        return Some(OrbMode::Starting { step, steps });
    }
    if !v.approvals.is_empty() {
        return Some(OrbMode::Asking);
    }
    if recent(anim.shake_at, SHAKE) {
        return Some(OrbMode::NotYou);
    }
    if v.capturing {
        return Some(if recent(anim.flash_at, FLASH) { OrbMode::Flash } else { OrbMode::Hearing });
    }
    if v.speaking {
        return Some(OrbMode::Speaking);
    }
    if v.thinking {
        return Some(OrbMode::Thinking);
    }
    if let Some(j) = v.jobs.iter().rev().find(|j| j.state == JobState::Running) {
        return Some(OrbMode::Working { task: j.task.clone(), seconds: now.duration_since(j.started).as_secs() });
    }
    Some(OrbMode::Listening)
}

/// What's written beside the orb (and what screen readers are told).
pub fn words(mode: &OrbMode, v: &VoiceView) -> String {
    let key = v.config.push_to_talk_key.as_str();
    match mode {
        OrbMode::Starting { step, steps } if *steps > 0 => {
            trf("Starting voice… loading models {n} of {total}", &[("n", &step.to_string()), ("total", &steps.to_string())])
        }
        OrbMode::Starting { .. } => tr("Starting voice…"),
        OrbMode::Error(e) => e.clone(),
        OrbMode::Asking => tr("Waiting for your yes or no"),
        OrbMode::NotYou => tr("Not your voice"),
        OrbMode::Hearing | OrbMode::Flash => {
            if v.partial.trim().is_empty() {
                tr("Hearing you…")
            } else {
                v.partial.trim().to_string()
            }
        }
        OrbMode::Speaking => trf("Speaking · tap or press {key} to interrupt", &[("key", key)]),
        OrbMode::Thinking => tr("Thinking…"),
        OrbMode::Working { task, seconds } => trf(
            "Opus is working on: {task} · {time}",
            &[("task", &crate::voice_orb::short(task, 60)), ("time", &format!("{}:{:02}", seconds / 60, seconds % 60))],
        ),
        OrbMode::Listening => match v.config.listen_mode {
            crate::voice_ui::ListenMode::WakeWord => trf("Listening · hold {key} or say “{word}”", &[("key", key), ("word", &v.config.wake_word)]),
            crate::voice_ui::ListenMode::Always => tr("Listening · just talk"),
            crate::voice_ui::ListenMode::PushToTalk => trf("Ready · hold {key} to talk", &[("key", key)]),
        },
    }
}

/// `text` cut to `max` characters, with an ellipsis.
pub fn short(text: &str, max: usize) -> String {
    let t = text.trim();
    if t.chars().count() <= max { t.to_string() } else { format!("{}…", t.chars().take(max).collect::<String>().trim_end()) }
}

/// A slow breath, 0–1.
pub fn breath(time: f64) -> f32 {
    ((time * 1.6).sin() as f32 * 0.5 + 0.5).powf(1.4)
}

/// Keep the loudness history (called every frame from `voice_ui::tick`).
pub fn history(voice: &mut Voice) {
    let (i, o) = (voice.view.level_in, voice.view.level_out);
    for (q, x) in [(&mut voice.orb.level_in, i), (&mut voice.orb.level_out, o)] {
        q.push_back(x.clamp(0.0, 1.0));
        while q.len() > HISTORY {
            q.pop_front();
        }
    }
}

/// The bars' heights for the newest `BARS` loudness samples, `r` the orb's radius: louder is
/// taller, and silence still shows a little.
pub fn bar_heights(levels: &VecDeque<f32>, r: f32) -> Vec<f32> {
    let newest: Vec<f32> = levels.iter().rev().take(BARS).copied().collect();
    (0..BARS)
        .map(|i| {
            // The middle bars follow the newest samples; the outer ones the older, smaller.
            let k = [5, 3, 1, 0, 2, 4, 6][i];
            let x = newest.get(k).copied().unwrap_or(0.0);
            let taper = 1.0 - (i as f32 - (BARS as f32 - 1.0) / 2.0).abs() / BARS as f32;
            (0.18 + 0.82 * x) * 1.5 * r * taper
        })
        .collect()
}

/// The orb's colour for a mode.
fn colour(t: &Tokens, mode: &OrbMode) -> Color32 {
    match mode {
        OrbMode::Error(_) => t.danger,
        OrbMode::Hearing | OrbMode::Flash => t.danger,
        OrbMode::Working { .. } => t.warning,
        OrbMode::NotYou => t.text_faint,
        OrbMode::Asking | OrbMode::Speaking | OrbMode::Thinking => t.accent,
        OrbMode::Starting { .. } => t.text_dim,
        OrbMode::Listening => t.success,
    }
}

/// Draw the orb (and the question card above it) over everything, at the bottom centre.
pub fn show(app: &mut CraftApp, ctx: &egui::Context) {
    let now = Instant::now();
    let Some(mode) = mode(&app.voice.view, &app.voice.orb, now) else { return };
    let t = Tokens::get(ctx);
    let words = words(&mode, &app.voice.view);
    let notices = app.voice.orb.notices_now(now);
    let mut actions: Vec<VoiceAction> = Vec::new();
    let mut open_conversation = false;
    let conversation_showing = app.voice.panel_open
        || (!app.beginner()
            && app.state.screen == crate::state::Screen::Workbench
            && app.state.bottom_open
            && app.state.bottom == crate::state::BottomTab::Voice);
    egui::Area::new(egui::Id::new("voice-orb"))
        .order(egui::Order::Foreground)
        .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -18.0))
        .interactable(true)
        .show(ctx, |ui| {
            ui.set_max_width(460.0);
            ui.vertical_centered(|ui| {
                // The question waiting for an answer, pinned above the orb (unless the conversation,
                // which has it too, is showing).
                if mode == OrbMode::Asking
                    && !conversation_showing
                    && let Some(a) = app.voice.view.approvals.first()
                {
                    egui::Frame::new()
                        .fill(t.card)
                        .stroke(egui::Stroke::new(1.5, t.accent))
                        .corner_radius(t.r_lg())
                        .inner_margin(egui::Margin::same(12))
                        .shadow(egui::epaint::Shadow { offset: [0, 4], blur: 16, spread: 0, color: t.shadow })
                        .show(ui, |ui| {
                            ui.set_max_width(420.0);
                            crate::voice_ui::approval_card(ui, &t, a, &mut actions);
                        });
                    ui.add_space(8.0);
                }
                for n in &notices {
                    let g = widgets::label_galley(ui, n, theme::CAPTION, t.text_dim);
                    let (r, _) = ui.allocate_exact_size(g.size() + Vec2::new(20.0, 10.0), egui::Sense::hover());
                    ui.painter().rect_filled(r, t.radius, t.card);
                    ui.painter().galley(r.center() - g.size() / 2.0, g, t.text_dim);
                    ui.add_space(6.0);
                }
                let r = capsule(ui, &t, &mode, &words, &app.voice, now);
                if r.clicked() {
                    if mode == OrbMode::Speaking {
                        actions.push(VoiceAction::StopSpeaking);
                    } else {
                        open_conversation = true;
                    }
                }
            });
        });
    for a in actions {
        app.voice.act(a);
    }
    if open_conversation {
        crate::voice_ui::show_conversation(app);
    }
    // Redraw only while something moves: quickly while it hears, speaks, thinks or flashes;
    // slowly while it breathes.
    let quick = matches!(mode, OrbMode::Hearing | OrbMode::Speaking | OrbMode::Thinking | OrbMode::Flash | OrbMode::NotYou | OrbMode::Starting { .. });
    ctx.request_repaint_after(if quick { widgets::FRAME / 2 } else { Duration::from_millis(80) });
}

/// The capsule: the orb on the left, the words on the right. Its response is the click.
fn capsule(ui: &mut Ui, t: &Tokens, mode: &OrbMode, words: &str, voice: &Voice, now: Instant) -> egui::Response {
    let col = colour(t, mode);
    let g = widgets::label_galley(ui, &short(words, 90), theme::SMALL, t.text);
    let size = Vec2::new(2.0 * R + 26.0 + g.size().x.min(380.0), 2.0 * R + 12.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    let time = ui.input(|i| i.time);
    // A grey shake for someone else's voice.
    let shake = match (mode, voice.orb.shake_at) {
        (OrbMode::NotYou, Some(at)) => {
            let k = 1.0 - (now.duration_since(at).as_secs_f32() / SHAKE.as_secs_f32()).min(1.0);
            (time * 46.0).sin() as f32 * 5.0 * k
        }
        _ => 0.0,
    };
    let rect = rect.translate(Vec2::new(shake, 0.0));
    let hover = widgets::hover_t(ui, &resp);
    let p = ui.painter();
    p.rect(
        rect,
        egui::CornerRadius::same((rect.height() / 2.0) as u8),
        theme::mix(t.card, t.raised, hover),
        egui::Stroke::new(1.0, col.gamma_multiply(0.55)),
        egui::StrokeKind::Inside,
    );
    let c = Pos2::new(rect.left() + 6.0 + R, rect.center().y);
    paint_orb(p, t, mode, c, voice, now, time);
    p.galley(Pos2::new(c.x + R + 12.0, rect.center().y - g.size().y / 2.0), g, t.text);
    widgets::focus_ring(ui, &resp, rect);
    let name = trf("Voice: {state}", &[("state", words)]);
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, name.clone()));
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// The orb itself, at `c`.
fn paint_orb(p: &egui::Painter, t: &Tokens, mode: &OrbMode, c: Pos2, voice: &Voice, now: Instant, time: f64) {
    let col = colour(t, mode);
    let breath = breath(time);
    match mode {
        OrbMode::Listening => {
            p.circle_filled(c, R * (0.55 + 0.1 * breath), col.gamma_multiply(0.12 + 0.12 * breath));
            p.circle_stroke(c, R * (0.78 + 0.12 * breath), egui::Stroke::new(2.0, col.gamma_multiply(0.45 + 0.4 * breath)));
        }
        OrbMode::Flash => {
            let k = voice.orb.flash_at.map_or(1.0, |at| (now.duration_since(at).as_secs_f32() / FLASH.as_secs_f32()).min(1.0));
            p.circle_filled(c, R * 0.85, col.gamma_multiply(0.9 * (1.0 - k) + 0.2));
            p.circle_stroke(c, R * (0.8 + 0.6 * k), egui::Stroke::new(2.5, col.gamma_multiply(1.0 - k)));
        }
        OrbMode::Hearing | OrbMode::Speaking => {
            let levels = if *mode == OrbMode::Hearing { &voice.orb.level_in } else { &voice.orb.level_out };
            p.circle_filled(c, R, col.gamma_multiply(0.14));
            let heights = bar_heights(levels, R);
            let w = 2.6;
            let gap = 1.8;
            let total = heights.len() as f32 * w + (heights.len() as f32 - 1.0) * gap;
            for (i, h) in heights.iter().enumerate() {
                let x = c.x - total / 2.0 + i as f32 * (w + gap) + w / 2.0;
                let h = h.min(R * 1.7);
                p.line_segment([Pos2::new(x, c.y - h / 2.0), Pos2::new(x, c.y + h / 2.0)], egui::Stroke::new(w, col));
            }
        }
        OrbMode::Thinking => {
            p.circle_stroke(c, R * 0.8, egui::Stroke::new(1.5, col.gamma_multiply(0.3)));
            for i in 0..3 {
                let a = time as f32 * 4.0 + i as f32 * std::f32::consts::TAU / 3.0;
                let d = Pos2::new(c.x + a.cos() * R * 0.55, c.y + a.sin() * R * 0.55);
                p.circle_filled(d, 2.8 - i as f32 * 0.5, col);
            }
        }
        OrbMode::Working { .. } => {
            let pulse = ((time * 3.0).sin() as f32 * 0.5 + 0.5).powf(1.2);
            p.circle_filled(c, R * (0.55 + 0.25 * pulse), col.gamma_multiply(0.35 + 0.4 * pulse));
            p.circle_stroke(c, R * 0.9, egui::Stroke::new(2.0, col.gamma_multiply(0.6)));
        }
        OrbMode::Asking => {
            p.circle_filled(c, R * 0.85, col.gamma_multiply(0.2 + 0.15 * breath));
            p.circle_stroke(c, R * 0.85, egui::Stroke::new(2.0, col));
            p.text(c, egui::Align2::CENTER_CENTER, "?", egui::FontId::proportional(16.0), col);
        }
        OrbMode::NotYou => {
            p.circle_filled(c, R * 0.75, col.gamma_multiply(0.35));
            p.circle_stroke(c, R * 0.75, egui::Stroke::new(1.5, col));
        }
        OrbMode::Error(_) => {
            p.circle_filled(c, R * 0.8, col.gamma_multiply(0.85));
            p.text(c, egui::Align2::CENTER_CENTER, "!", egui::FontId::proportional(17.0), t.on_accent);
        }
        OrbMode::Starting { step, steps } => {
            // A turning arc, and the share of the models loaded as a ring.
            let a0 = time as f32 * 3.5;
            let arc: Vec<Pos2> = (0..=16).map(|i| a0 + i as f32 * 0.12).map(|a| Pos2::new(c.x + a.cos() * R * 0.75, c.y + a.sin() * R * 0.75)).collect();
            p.add(egui::Shape::line(arc, egui::Stroke::new(2.5, t.accent)));
            if *steps > 0 {
                let share = *step as f32 / *steps as f32;
                let ring: Vec<Pos2> = (0..=48)
                    .map(|i| -std::f32::consts::FRAC_PI_2 + i as f32 / 48.0 * share * std::f32::consts::TAU)
                    .map(|a| Pos2::new(c.x + a.cos() * R, c.y + a.sin() * R))
                    .collect();
                p.add(egui::Shape::line(ring, egui::Stroke::new(2.0, col.gamma_multiply(0.7))));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice_ui::{OpusJob, VoiceView};

    #[test]
    fn the_orb_says_what_voice_is_doing_most_pressing_first() {
        let now = Instant::now();
        let anim = OrbAnim::default();
        let mut v = VoiceView::default();
        assert_eq!(mode(&v, &anim, now), None, "off: no orb");
        v.starting = true;
        v.loading = Some((2, 5, "speech recognition".into()));
        assert_eq!(mode(&v, &anim, now), Some(OrbMode::Starting { step: 2, steps: 5 }));
        assert_eq!(words(&OrbMode::Starting { step: 2, steps: 5 }, &v), "Starting voice… loading models 2 of 5");
        v.starting = false;
        v.running = true;
        v.ready = true;
        assert_eq!(mode(&v, &anim, now), Some(OrbMode::Listening));
        v.jobs.push(OpusJob {
            id: "j".into(),
            task: "Cut the trailer".into(),
            progress: None,
            latest: String::new(),
            started: now - Duration::from_secs(75),
            state: JobState::Running,
        });
        let working = mode(&v, &anim, now).unwrap();
        assert_eq!(working, OrbMode::Working { task: "Cut the trailer".into(), seconds: 75 });
        assert_eq!(words(&working, &v), "Opus is working on: Cut the trailer · 1:15");
        v.thinking = true;
        assert_eq!(mode(&v, &anim, now), Some(OrbMode::Thinking));
        v.speaking = true;
        assert_eq!(mode(&v, &anim, now), Some(OrbMode::Speaking));
        v.capturing = true;
        assert_eq!(mode(&v, &anim, now), Some(OrbMode::Hearing));
        let flash = OrbAnim { flash_at: Some(now), ..Default::default() };
        assert_eq!(mode(&v, &flash, now), Some(OrbMode::Flash));
        let shake = OrbAnim { shake_at: Some(now), ..Default::default() };
        assert_eq!(mode(&v, &shake, now), Some(OrbMode::NotYou));
        v.approvals.push(crate::voice_demo::sample_approval("a"));
        assert_eq!(mode(&v, &shake, now), Some(OrbMode::Asking));
        v.error = Some("The microphone is unavailable".into());
        let err = OrbAnim { error_at: Some(now), ..Default::default() };
        assert_eq!(mode(&v, &err, now), Some(OrbMode::Error("The microphone is unavailable".into())));
        assert_eq!(mode(&v, &err, now + ERROR), Some(OrbMode::Asking), "an old error gives way");
    }

    #[test]
    fn the_bars_follow_the_loudness() {
        let quiet: VecDeque<f32> = std::iter::repeat_n(0.0, HISTORY).collect();
        let loud: VecDeque<f32> = std::iter::repeat_n(0.9, HISTORY).collect();
        let (q, l) = (bar_heights(&quiet, R), bar_heights(&loud, R));
        assert_eq!((q.len(), l.len()), (BARS, BARS));
        assert!(q.iter().zip(&l).all(|(a, b)| b > a), "{q:?} {l:?}");
        assert!(q.iter().all(|h| *h > 0.0), "silence still shows a little");
        // The middle bar is the newest sample.
        let mut rising: VecDeque<f32> = std::iter::repeat_n(0.0, HISTORY).collect();
        rising.push_back(1.0);
        let h = bar_heights(&rising, R);
        assert!(h[3] > h[2] && h[3] > h[4] && h[3] > q[3], "the newest is in the middle: {h:?}");
        assert!((breath(0.0) - breath(0.0)).abs() < f32::EPSILON && (0.0..=1.0).contains(&breath(12.3)));
    }
}
