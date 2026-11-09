//! The Workbench follows the open project (#1107): while a project is open, the Workbench shows
//! it. Its investigation (the `.cany` folder its builds record findings in) is opened in the
//! session, and so is its original when it's a file or folder on this computer, both on workers,
//! so Evidence, Findings and Unknowns show what the builder found. The original isn't analysed
//! until the person asks (Analyse): opening it is quick, analysing a large program is not.
//! The console's Agent tab shows the project's conversation, Jobs its build's activity, and Log
//! its build-activity.log.

use crate::i18n::{tr, trf};
use crate::state::Screen;
use crate::theme::{self, Tokens};
use crate::{CraftApp, Done, icons, widgets};
use craft_anything_ai::rebuild::activity::LineKind;
use craft_anything_engine::evidence::ToolError;
use craft_anything_engine::project::Project;
use craft_anything_engine::workspace::{ACTIVITY_LOG, TargetRef, Workspace};
use craft_anything_engine::{CallOptions, Caller, Engine};
use egui::{Margin, RichText, Ui, Vec2};
use serde_json::json;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How much of the end of the build log is read, and how many of its lines are kept.
const LOG_TAIL_BYTES: u64 = 256 * 1024;
const LOG_LINES: usize = 3000;
/// How often the build log is looked at while it's shown.
const LOG_EVERY: Duration = Duration::from_secs(1);

/// What following opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    /// The project's investigation (its findings, evidence and open questions).
    Investigation,
    /// The original it rebuilds.
    Original,
}

/// Where opening one part stands.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Step {
    #[default]
    Idle,
    Working,
    /// Open; what it holds, in words.
    Done(String),
    /// There's nothing to open (no findings saved yet, or no original on this computer).
    Nothing,
    /// Another project's build is using the shared investigation now.
    Busy,
    /// Looking inside the original needs Deep mode.
    DeepNeeded,
    Failed(String),
}

/// The project the Workbench follows, and what's been opened for it.
#[derive(Debug, Default)]
pub struct Follow {
    pub root: Option<PathBuf>,
    pub name: String,
    /// The person left the project's context for this project.
    pub off: Option<PathBuf>,
    pub investigation: Step,
    pub original: Step,
    /// The person asked for the original to be analysed: the Workbench's lists fill only then.
    pub analyse: bool,
    /// The end of the project's build log.
    pub log: Vec<String>,
    log_len: Option<u64>,
    log_job: Option<u64>,
    log_at: Option<Instant>,
}

impl Follow {
    pub fn on(&self) -> bool {
        self.root.is_some()
    }

    /// The lists wait for Analyse: the original was opened for the project, not by the person.
    pub fn holds_lists(&self) -> bool {
        self.root.is_some() && matches!(self.original, Step::Done(_)) && !self.analyse
    }

    /// A job failed: if it was reading the log, the next look starts another.
    pub(crate) fn job_failed(&mut self, job: u64) {
        if self.log_job == Some(job) {
            self.log_job = None;
        }
    }
}

/// Open the investigation folder `dir` in the session, in place of any other project's. `None`:
/// the project has none yet.
fn open_investigation(engine: &Engine, dir: &Path) -> Result<Option<usize>, ToolError> {
    if !dir.join("project.toml").is_file() {
        return Ok(None);
    }
    let project = Project::open(dir)?;
    let mut session = engine.session();
    session.label("Open the project's investigation");
    if session.project.as_ref().is_some_and(|p| p.root != dir) {
        session.close_project();
    }
    session.open_project(project)?;
    Ok(Some(session.ledger_len()))
}

/// The last lines of the build log, unless it hasn't grown since `known` bytes.
fn read_tail(path: &Path, known: Option<u64>) -> Option<(u64, Vec<String>)> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    if known == Some(len) {
        return None;
    }
    let start = len.saturating_sub(LOG_TAIL_BYTES);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    f.take(LOG_TAIL_BYTES).read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<String> = text.lines().skip(usize::from(start > 0)).map(str::to_string).collect();
    let extra = lines.len().saturating_sub(LOG_LINES);
    lines.drain(..extra);
    Some((len, lines))
}

impl CraftApp {
    /// Follow the open project in the Workbench (called as it's drawn): a project newly open
    /// there gets its investigation and its original opened on workers; none, today's Workbench.
    pub fn follow_project(&mut self) {
        let open = self.workspace.as_ref().map(|w| w.root.clone());
        // Leaving one project's context lasts until another project is opened.
        if self.follow.off.is_some() && self.follow.off != open {
            self.follow.off = None;
        }
        let want = open.filter(|r| self.follow.off.as_ref() != Some(r));
        if want == self.follow.root {
            // The other project's build that held the findings has finished: open them now.
            if self.follow.investigation == Step::Busy
                && let Some(ws) = self.workspace.clone().filter(|w| !self.runs.running().iter().any(|r| r.root != w.root))
            {
                self.open_project_investigation(&ws);
            }
            return;
        }
        self.follow = Follow { off: self.follow.off.take(), ..Default::default() };
        let Some(ws) = self.workspace.clone().filter(|_| want.is_some()) else { return };
        self.follow.root = Some(ws.root.clone());
        self.follow.name = ws.manifest.name.clone();
        // The lists and views were of whatever was open before.
        self.state.lists.clear();
        self.state.views.clear();
        self.state.selection = None;
        self.state.selected_evidence = None;
        self.open_project_investigation(&ws);
        self.open_project_original(&ws);
    }

    /// Open the project's investigation in the session (on a worker), unless another project's
    /// build is using it now: its findings would go to the wrong project.
    fn open_project_investigation(&mut self, ws: &Workspace) {
        if self.runs.running().iter().any(|r| r.root != ws.root) {
            self.follow.investigation = Step::Busy;
            return;
        }
        self.follow.investigation = Step::Working;
        let (engine, root, dir) = (self.engine.clone(), ws.root.clone(), ws.investigation_dir());
        self.jobs.spawn(trf("Open {name}'s findings", &[("name", &ws.manifest.name)]), true, move |_| {
            let result = open_investigation(&engine, &dir).map(|n| n.map(|n| trf("{n} evidence records", &[("n", &n.to_string())])));
            Ok(Done::Followed { root, part: Part::Investigation, result })
        });
    }

    /// Open the original the project rebuilds (core_target_open), when it's on this computer and
    /// Deep mode is on. It isn't analysed until the person asks.
    fn open_project_original(&mut self, ws: &Workspace) {
        let TargetRef::Local { path } = &ws.manifest.target else {
            self.follow.original = Step::Nothing;
            return;
        };
        let clean_room = self.engine.try_session().map_or(!self.settings.deep_mode, |s| s.clean_room());
        if clean_room {
            self.follow.original = Step::DeepNeeded;
            return;
        }
        self.follow.original = Step::Working;
        let (engine, root, path) = (self.engine.clone(), ws.root.clone(), path.clone());
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
        self.jobs.spawn(trf("Open {name}", &[("name", &name)]), true, move |j| {
            let opts = CallOptions::new(Caller::Ui).with_cancel(j.cancel.clone());
            let result = engine.call("core_target_open", json!({"path": path}), opts).map(|o| {
                let v = o.to_json();
                Some(v["result"]["name"].as_str().or(v["name"].as_str()).unwrap_or(&name).to_string())
            });
            Ok(Done::Followed { root, part: Part::Original, result })
        });
    }

    /// Stop showing the open project in the Workbench (until another project is opened).
    pub fn leave_project_context(&mut self) {
        self.follow.off = self.follow.root.clone();
        self.follow.root = None;
        self.follow.analyse = false;
    }

    /// Analyse the followed project's original: the Workbench's lists fill from here on.
    pub fn analyse_followed(&mut self) {
        self.follow.analyse = true;
        self.ensure_rail_loaded();
    }

    /// Opening a part of the followed project finished.
    pub(crate) fn followed(&mut self, root: PathBuf, part: Part, result: Result<Option<String>, ToolError>) {
        if self.follow.root.as_ref() != Some(&root) {
            return;
        }
        let step = match result {
            Ok(Some(words)) => Step::Done(words),
            Ok(None) => Step::Nothing,
            Err(e) if craft_anything_engine::clean_room::is_refusal(&e) => Step::DeepNeeded,
            Err(e) => Step::Failed(e.for_people()),
        };
        match part {
            Part::Investigation => self.follow.investigation = step,
            Part::Original => {
                self.follow.original = step;
                self.state.lists.clear();
                self.state.views.clear();
                if self.state.rail == crate::state::RailKind::Targets {
                    self.state.rail = crate::state::RailKind::Functions;
                }
            }
        }
        self.mark_dirty();
    }

    /// Read the end of the followed project's build log again, at most once a second.
    fn tail_project_log(&mut self, ctx: &egui::Context) {
        let Some(root) = self.follow.root.clone() else { return };
        ctx.request_repaint_after(LOG_EVERY);
        if self.follow.log_job.is_some() || self.follow.log_at.is_some_and(|at| at.elapsed() < LOG_EVERY) {
            return;
        }
        self.follow.log_at = Some(Instant::now());
        let known = self.follow.log_len;
        self.follow.log_job = Some(self.jobs.spawn("Read the build log", false, move |_| {
            let tail = read_tail(&root.join(ACTIVITY_LOG), known);
            Ok(Done::LogTail { root, tail })
        }));
    }

    pub(crate) fn log_tail_read(&mut self, root: PathBuf, tail: Option<(u64, Vec<String>)>) {
        self.follow.log_job = None;
        if self.follow.root.as_ref() != Some(&root) {
            return;
        }
        if let Some((len, lines)) = tail {
            self.follow.log_len = Some(len);
            self.follow.log = lines;
        }
    }
}

fn step_words(step: &Step, part: Part) -> Option<(String, bool)> {
    let investigation = part == Part::Investigation;
    Some(match step {
        Step::Idle => return None,
        Step::Working => (tr(if investigation { "Opening the findings…" } else { "Opening the original…" }), false),
        Step::Done(words) => (words.clone(), false),
        Step::Nothing => (tr(if investigation { "No findings saved yet" } else { "No original on this computer" }), false),
        Step::Busy => (tr("Another project's build is using the findings"), true),
        Step::DeepNeeded => (tr("The original needs Deep mode"), true),
        Step::Failed(why) => (why.clone(), true),
    })
}

/// The bar over the Workbench while it follows a project: its name, what's open, Analyse, and
/// the ways back to the project page or out of its context.
pub fn header(app: &mut CraftApp, ui: &mut Ui) {
    let t = Tokens::get(ui.ctx());
    let mut analyse = false;
    let mut page = false;
    let mut leave = false;
    let mut deep = false;
    ui.horizontal(|ui| {
        icons::show(ui, "hammer", 16.0, t.accent);
        ui.label(RichText::new(trf("Project: {name}", &[("name", &app.follow.name)])).font(theme::semibold(theme::SMALL)).color(t.text));
        for (step, part) in [(&app.follow.investigation, Part::Investigation), (&app.follow.original, Part::Original)] {
            if let Some((words, warn)) = step_words(step, part) {
                if *step == Step::Working {
                    ui.add(widgets::Spinner::new().size(12.0).color(t.accent));
                }
                let r = widgets::pill(ui, &words, if warn { t.warning } else { t.text_dim }, t.raised);
                if *step == Step::DeepNeeded && r.on_hover_text(tr("Turn on Deep mode")).interact(egui::Sense::click()).clicked() {
                    deep = true;
                }
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if widgets::secondary(ui, Some("x"), "Leave project").on_hover_text(tr("Use the Workbench without this project.")).clicked() {
                leave = true;
            }
            if widgets::secondary(ui, Some("hammer"), "Project page").clicked() {
                page = true;
            }
            if app.follow.holds_lists()
                && widgets::primary_icon(ui, Some("microscope"), "Analyse", true)
                    .on_hover_text(tr("Analyse the original to fill the lists. A large program takes a while."))
                    .clicked()
            {
                analyse = true;
            }
        });
    });
    if analyse {
        app.analyse_followed();
    }
    if page {
        app.state.screen = Screen::Project;
    }
    if leave {
        app.leave_project_context();
    }
    if deep {
        app.state.modal = Some(crate::state::Modal::DeepMode);
    }
}

/// The lists wait for Analyse (shown where a list would be).
pub fn analyse_first(app: &mut CraftApp, ui: &mut Ui) {
    widgets::empty_state(ui, "microscope", "Not analysed yet", "Analysing the original can take a while. What the builder found is under Evidence already.");
    ui.vertical_centered(|ui| {
        if widgets::primary_icon(ui, Some("microscope"), "Analyse", true).clicked() {
            app.analyse_followed();
        }
    });
}

/// The console's Agent tab for the followed project: its conversation, and a box that sends a
/// note to the builder while it works, or asks for a change otherwise.
pub fn agent(app: &mut CraftApp, ui: &mut Ui) {
    let Some(ws) = app.workspace.clone().filter(|w| app.follow.root.as_ref() == Some(&w.root)) else {
        crate::chat::conversation(app, ui, true);
        return;
    };
    let t = Tokens::get(ui.ctx());
    let box_h = 40.0;
    let feed_h = (ui.available_height() - box_h - 6.0).max(40.0);
    ui.allocate_ui_with_layout(Vec2::new(ui.available_width(), feed_h), egui::Layout::top_down(egui::Align::Min), |ui| {
        ui.set_min_height(feed_h);
        egui::ScrollArea::vertical().id_salt("wb-project-feed").auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
            if app.state.feed.is_empty() {
                ui.label(RichText::new(tr("Nothing said in this project yet.")).color(t.text_faint));
            }
            crate::project::feed(app, ui, &ws);
        });
    });
    ui.add_space(6.0);
    let running = app.runs.is_running(&ws.root);
    let mut send = false;
    ui.horizontal(|ui| {
        let hint = if running { "A note for the builder…" } else { "Ask for a change…" };
        let r = ui.add(
            egui::TextEdit::singleline(&mut app.state.project_input)
                .hint_text(tr(hint))
                .desired_width((ui.available_width() - 90.0).max(80.0))
                .frame(widgets::text_field_frame_compact(&t)),
        );
        let r = widgets::named(r, "Message to the builder");
        if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            send = true;
        }
        if widgets::primary_icon(ui, Some("send"), "Send", !app.state.project_input.trim().is_empty()).clicked() {
            send = true;
        }
    });
    if send {
        app.send_message();
    }
}

/// The console's Jobs tab, above the app's own jobs: the followed project's build, live.
pub fn run_activity(app: &mut CraftApp, ui: &mut Ui) {
    const LINE: f32 = 18.0;
    let Some(root) = app.follow.root.clone() else { return };
    let t = Tokens::get(ui.ctx());
    let Some(run) = app.runs.get(&root) else {
        ui.label(RichText::new(tr("No build of this project is running in this window.")).color(t.text_faint).size(theme::CAPTION));
        ui.add_space(6.0);
        return;
    };
    let running = run.is_running();
    let now = crate::project::now_words(app, &run.activity_now());
    let count = run.activity_len();
    ui.horizontal(|ui| {
        if running {
            ui.add(widgets::Spinner::new().size(12.0).color(t.accent));
        }
        let title = if running { tr("This project's build") } else { tr("What happened in this build") };
        ui.label(RichText::new(title).font(theme::medium(theme::CAPTION)).color(t.text));
        if let Some(n) = now {
            ui.add(egui::Label::new(RichText::new(n).color(t.text_dim).size(theme::CAPTION)).truncate());
        }
    });
    if count > 0 {
        egui::Frame::new().fill(t.code_bg).corner_radius(t.r()).inner_margin(Margin::symmetric(8, 4)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.spacing_mut().interact_size.y = LINE;
            egui::ScrollArea::vertical().id_salt("wb-run-activity").max_height(160.0).auto_shrink([false, true]).stick_to_bottom(true).show_rows(
                ui,
                LINE,
                count,
                |ui, range| {
                    for line in run.activity_lines(range) {
                        let colour = match line.kind {
                            LineKind::Failed | LineKind::Problem => t.warning,
                            LineKind::Done => t.success,
                            LineKind::Stage => t.accent_ink,
                            _ => t.text_faint,
                        };
                        ui.horizontal(|ui| {
                            ui.set_height(LINE);
                            ui.label(RichText::new(crate::usage::duration_words(line.at)).font(theme::mono(theme::CAPTION)).color(t.text_faint));
                            ui.label(RichText::new(line.kind.tag()).font(theme::mono(theme::CAPTION)).color(colour));
                            ui.add(egui::Label::new(RichText::new(&line.text).font(theme::mono(theme::CAPTION)).color(t.text)).truncate())
                                .on_hover_text(&line.text);
                        });
                    }
                },
            );
        });
    }
    if running {
        ui.ctx().request_repaint_after(Duration::from_secs(1));
    }
    ui.add_space(8.0);
}

/// The console's Log tab for the followed project: the end of its build-activity.log, live.
pub fn log(app: &mut CraftApp, ui: &mut Ui) {
    let ctx = ui.ctx().clone();
    app.tail_project_log(&ctx);
    let t = Tokens::get(ui.ctx());
    if app.follow.log.is_empty() {
        ui.label(RichText::new(tr("Nothing in this project's build log yet.")).color(t.text_faint));
        return;
    }
    let row_h = 16.0;
    egui::ScrollArea::both().id_salt("wb-project-log").auto_shrink([false, false]).stick_to_bottom(true).show_rows(
        ui,
        row_h,
        app.follow.log.len(),
        |ui, range| {
            for line in app.follow.log.get(range).unwrap_or_default() {
                ui.label(RichText::new(line).font(theme::mono(theme::CAPTION)).color(t.text_dim));
            }
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_tail_reads_whole_lines_and_skips_an_unchanged_file() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join(ACTIVITY_LOG);
        assert!(read_tail(&p, None).is_none(), "no log yet");
        let mut text = String::new();
        for i in 0..20_000 {
            text.push_str(&format!("line {i}\n"));
        }
        std::fs::write(&p, &text).unwrap();
        let (len, lines) = read_tail(&p, None).unwrap();
        assert_eq!(len, text.len() as u64);
        assert_eq!(lines.last().map(String::as_str), Some("line 19999"));
        assert!(lines.len() <= LOG_LINES && lines.iter().all(|l| l.starts_with("line ")), "only whole lines");
        assert!(read_tail(&p, Some(len)).is_none(), "unchanged");
    }
}
