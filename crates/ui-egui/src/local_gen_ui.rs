//! Settings → Engines → Local generation: where the picture, 3D and audio servers are, how to
//! start them, background removal, Blender, voices and the sound decoder, with a status line for
//! each and Check / Start. The
//! checking and starting are the engine's tools (`local_generation_status`,
//! `local_generation_start`), run on workers.

use crate::i18n::tr;
use crate::theme::{self, Tokens};
use crate::{CraftApp, icons, widgets};
use egui::{RichText, Ui};
use serde_json::{Value, json};

/// What the section shows: the last status, and whether a check or a start is under way.
#[derive(Debug, Default)]
pub struct Panel {
    pub status: Option<Value>,
    pub checking: bool,
    pub starting: bool,
    asked: bool,
}

impl CraftApp {
    /// Look at the local generation servers and helpers again (local_generation_status).
    pub fn check_local_generation(&mut self) {
        self.local_gen.checking = true;
        self.local_gen.asked = true;
        self.act("local_generation_status", json!({}), tr("Check local generation"));
    }

    /// Start the servers that aren't running (local_generation_start).
    pub fn start_local_generation(&mut self) {
        self.local_gen.starting = true;
        // Shown in Jobs with its progress: starting can take minutes while the models load.
        let engine = self.engine.clone();
        self.jobs.spawn(tr("Start local generation"), true, move |j| {
            let progress = j.reporter();
            let opts = craft_anything_engine::CallOptions::new(craft_anything_engine::Caller::Ui)
                .with_consent(craft_anything_engine::Consent::Granted)
                .with_cancel(j.cancel.clone())
                .with_progress(&progress);
            let result = engine.call("local_generation_start", json!({}), opts).map(|o| o.to_json().get("result").cloned().unwrap_or(Value::Null));
            Ok(crate::Done::Acted { tool: "local_generation_start", result })
        });
    }

    /// A check or start finished.
    pub(crate) fn local_generation_done(&mut self, tool: &str, result: Result<Value, craft_anything_engine::evidence::ToolError>) {
        self.local_gen.checking = false;
        self.local_gen.starting = false;
        match result {
            Ok(v) if tool == "local_generation_start" => {
                self.local_gen.status = Some(v["status"].clone());
                let started = v["started"].as_array().map_or(0, Vec::len);
                self.toast(if started > 0 { tr("Local generation started.") } else { tr("Local generation was already running.") }, false);
            }
            Ok(v) => self.local_gen.status = Some(v),
            Err(e) => self.toast(e.for_people(), true),
        }
    }
}

/// One part's line: a dot (ready or not), its name and what's what.
fn part_line(ui: &mut Ui, name: &str, part: &Value) {
    let t = Tokens::get(ui.ctx());
    let ready = part["ready"].as_bool() == Some(true);
    ui.horizontal(|ui| {
        let (icon, col) = if ready { ("check", t.success) } else { ("triangle-alert", t.warning) };
        icons::show(ui, icon, 14.0, col);
        ui.label(RichText::new(tr(name)).font(theme::medium(theme::CAPTION)));
        let state = match part["state"].as_str() {
            Some("ready") => tr("Ready"),
            Some("not_running") => tr("Not running"),
            Some("missing_model") => tr("Missing model"),
            _ => tr("Not set up"),
        };
        ui.label(RichText::new(state).color(if ready { t.success } else { t.warning }).size(theme::CAPTION));
    });
    ui.add(egui::Label::new(RichText::new(part["detail"].as_str().unwrap_or_default()).color(t.text_faint).size(theme::CAPTION)).wrap());
}

/// A text setting (an address).
fn text_field(ui: &mut Ui, value: &mut String, name: &str) -> bool {
    let r = ui.add(
        egui::TextEdit::singleline(value)
            .desired_width((ui.available_width() - 34.0).clamp(120.0, 240.0))
            .font(theme::mono(12.0))
            .frame(widgets::text_field_frame_compact(&Tokens::get(ui.ctx()))),
    );
    widgets::named(r.clone(), name);
    r.changed()
}

/// The Local generation section of Settings → Engines.
pub fn section(app: &mut CraftApp, ui: &mut Ui, path_field: fn(&mut Ui, &mut Option<std::path::PathBuf>, &str, &str) -> bool) {
    let t = Tokens::get(ui.ctx());
    widgets::caption(ui, "Local generation");
    ui.add(
        egui::Label::new(
            RichText::new(tr(
                "Make pictures, 3D models, sounds, music and voices on this computer's graphics cards, for free: a ComfyUI server for pictures (Z-Image Turbo), one for 3D shapes (Hunyuan3D 2.1) and one for sounds and music (Stable Audio 3, ACE-Step 1.5), usually the 3D one; voices come from a Python with Qwen3-TTS. With one graphics card, give the servers the same address.",
            ))
            .color(t.text_faint)
            .size(theme::CAPTION),
        )
        .wrap(),
    );
    ui.add_space(6.0);
    if !app.local_gen.asked && app.local_gen.status.is_none() {
        app.check_local_generation();
    }
    match app.local_gen.status.clone() {
        Some(st) => {
            for (key, name) in [
                ("pictures", "Picture server"),
                ("shapes", "3D server"),
                ("background_removal", "Background removal"),
                ("blender", "Blender"),
                ("audio", "Sound and music"),
                ("voices", "Voices"),
                ("sound_decoder", "Sound decoder"),
            ] {
                part_line(ui, name, &st[key]);
            }
        }
        None if app.local_gen.checking => {
            ui.add(widgets::Spinner::new().size(14.0).color(t.accent));
        }
        None => {}
    }
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if widgets::secondary(ui, Some("refresh-cw"), "Check").clicked() && !app.local_gen.checking {
            app.check_local_generation();
        }
        let startable = app
            .local_gen
            .status
            .as_ref()
            .is_some_and(|s| ["pictures", "shapes", "audio"].iter().any(|k| s[*k]["state"] == "not_running" && s[*k]["startable"].as_bool() == Some(true)));
        if startable && widgets::secondary(ui, Some("play"), "Start").clicked() && !app.local_gen.starting {
            app.start_local_generation();
        }
        if app.local_gen.checking || app.local_gen.starting {
            ui.add(widgets::Spinner::new().size(14.0).color(t.accent));
        }
    });
    ui.add_space(10.0);
    let mut changed = false;
    let lg = &mut app.settings.local_gen;
    egui::Grid::new("local-gen-paths").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
        ui.label(RichText::new(tr("Picture server")).size(theme::CAPTION));
        changed |= text_field(ui, &mut lg.picture_url, "Picture server address");
        ui.end_row();
        ui.label(RichText::new(tr("3D server")).size(theme::CAPTION));
        changed |= text_field(ui, &mut lg.shape_url, "3D server address");
        ui.end_row();
        ui.label(RichText::new(tr("Start the picture server")).size(theme::CAPTION));
        changed |= path_field(ui, &mut lg.picture_start, "Start the picture server", "a script or program");
        ui.end_row();
        ui.label(RichText::new(tr("Start the 3D server")).size(theme::CAPTION));
        changed |= path_field(ui, &mut lg.shape_start, "Start the 3D server", "a script or program");
        ui.end_row();
        ui.label(RichText::new(tr("Background removal")).size(theme::CAPTION)).on_hover_text(tr("The Python that has rembg installed."));
        changed |= path_field(ui, &mut lg.bg_python, "Background removal", "python with rembg");
        ui.end_row();
        ui.label(RichText::new(tr("Blender")).size(theme::CAPTION));
        changed |= path_field(ui, &mut lg.blender, "Blender", "found automatically");
        ui.end_row();
        ui.label(RichText::new(tr("Sound and music")).size(theme::CAPTION)).on_hover_text(tr("Usually the 3D server's address: it starts with it."));
        changed |= text_field(ui, &mut lg.audio_url, "Sound and music server address");
        ui.end_row();
        ui.label(RichText::new(tr("Voices")).size(theme::CAPTION)).on_hover_text(tr("The Python that has Qwen3-TTS and Whisper installed."));
        changed |= path_field(ui, &mut lg.voice_python, "Voices", "python with Qwen3-TTS");
        ui.end_row();
        ui.label(RichText::new(tr("Sound decoder")).size(theme::CAPTION))
            .on_hover_text(tr("vgmstream-cli, to listen to the original's sounds in a game's own format."));
        changed |= path_field(ui, &mut lg.sound_decoder, "Sound decoder", "found automatically");
        ui.end_row();
    });
    if changed {
        app.settings_changed();
        // Checked again on Check, not on every key typed.
        app.local_gen.status = None;
    }
}
