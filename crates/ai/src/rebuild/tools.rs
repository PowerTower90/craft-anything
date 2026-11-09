//! Rebuild control as tools, registered from this layer next to the engine's own
//! ([`craft_anything_engine::Engine::shared`]): start a build (new or a follow-up), see whether
//! it's running, add a note while it works, and stop it. The conversation, questions and result
//! are the project tools' (`project_show`, `project_inbox_answer`, `project_play`…).

use super::*;
use craft_anything_engine::EngineRef;
use craft_anything_engine::questions::{self, AskedQuestion};
use craft_anything_engine::tool::{Ctx, Effects, Requires, ToolOutput, ToolSpec, Toolset, parse_params, schema_of};
use schemars::JsonSchema;
use serde::Deserialize;

/// The AI layer's tools, for [`craft_anything_engine::Engine::shared`].
pub fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            id: "rebuild_start",
            title: "Start a rebuild",
            summary: "Start building: a new project from a request and a target (a web address, an absolute path, or - for none), or a follow-up on an existing project. A coding agent studies, builds, checks and refines in the background; follow it with rebuild_status and project_show, answer its questions with project_inbox_answer. Up to `build.parallel` projects build at once (2 by default); more are queued and start on their own as others finish. It runs a coding agent that writes and builds code, so it needs consent.",
            beginner: "Starts making something.",
            toolset: Toolset::Projects,
            effects: Effects::LAUNCHES_PROCESS.union(Effects::EXECUTES_FOREIGN_CODE).union(Effects::WRITES_FILESYSTEM).union(Effects::ACCESSES_NETWORK),
            requires: Requires::Nothing,
            input_schema: schema_of::<StartParams>,
            run: rebuild_start,
        },
        ToolSpec {
            id: "rebuild_check_now",
            title: "Check a build now",
            summary: "Run the checks a builder turn gets on what a project has built now, without asking the builder anything: whether it builds and runs, and how close it is to the original. When it works the project becomes Ready (so it can be played or opened); otherwise the checks say what they found. For a build left Building or stopped part-way after the builder had finished (a turn that never closed, the app closed). It runs the project's own build, so it needs consent. Refused while the project is building.",
            beginner: "Checks what's built so far and marks it ready when it works.",
            toolset: Toolset::Projects,
            effects: Effects::LAUNCHES_PROCESS.union(Effects::EXECUTES_FOREIGN_CODE).union(Effects::WRITES_FILESYSTEM).union(Effects::ACCESSES_NETWORK),
            requires: Requires::Nothing,
            input_schema: schema_of::<ProjectParams>,
            run: rebuild_check_now,
        },
        ToolSpec {
            id: "rebuild_status",
            title: "Rebuild status",
            summary: "Whether a project's build is running in this session (or queued, and where), its status and note, the latest check, and how many questions are waiting. For a running build: `started_secs_ago`, `quiet_secs` (since its last event) and `now` (the action under way, in words), what the app's status row shows.",
            beginner: "Shows how a build is going.",
            toolset: Toolset::Projects,
            effects: Effects::READ_ONLY,
            requires: Requires::Nothing,
            input_schema: schema_of::<ProjectParams>,
            run: rebuild_status,
        },
        ToolSpec {
            id: "rebuild_activity_show",
            title: "Show a build's activity",
            summary: "What a project's builds did, line by line, newest last: each stage, every command and file the builder touched and how it ended, what it said, and what programs printed outside the conversation. Secrets are hidden. Works while a build runs and after. `limit` lines (200 by default, at most 2000), `offset` lines back from the newest.",
            beginner: "Shows the details of what a build has been doing.",
            toolset: Toolset::Projects,
            effects: Effects::READ_ONLY.union(Effects::CARRIES_TARGET_CONTENT),
            requires: Requires::Nothing,
            input_schema: schema_of::<ActivityParams>,
            run: rebuild_activity_show,
        },
        ToolSpec {
            id: "rebuild_note",
            title: "Add to a running build",
            summary: "Tell a running build something more; the agent reads it at its next step. With no build running, start a follow-up with rebuild_start instead.",
            beginner: "Adds information while it builds.",
            toolset: Toolset::Projects,
            effects: Effects::CHANGES_UI_STATE.union(Effects::WRITES_FILESYSTEM),
            requires: Requires::Nothing,
            input_schema: schema_of::<NoteParams>,
            run: rebuild_note,
        },
        ToolSpec {
            id: "rebuild_note_withdraw",
            title: "Take back a note",
            summary: "Take back a note given with rebuild_note (by the id it returned) while the agent hasn't read it yet; it returns the note's text. Once the agent has read it, it can't be taken back.",
            beginner: "Takes back something you added before the builder reads it.",
            toolset: Toolset::Projects,
            effects: Effects::CHANGES_UI_STATE,
            requires: Requires::Nothing,
            input_schema: schema_of::<WithdrawParams>,
            run: rebuild_note_withdraw,
        },
        ToolSpec {
            id: "rebuild_stop",
            title: "Stop a rebuild",
            summary: "Pause a project's running build. Its work so far is kept; a follow-up continues it.",
            beginner: "Pauses a build.",
            toolset: Toolset::Projects,
            effects: Effects::CHANGES_UI_STATE,
            requires: Requires::Nothing,
            input_schema: schema_of::<ProjectParams>,
            run: rebuild_stop,
        },
        ToolSpec {
            id: QUESTION_TOOL,
            title: "Ask about a build",
            summary: "Ask a question about a project's build (\"where are the latest files?\", \"why this engine?\") without starting a build or a round, and without changing anything. Claude Code answers from a copy of the builder's own session, which isn't kept, reading files only (none in Clean room); an AI with an API key answers with the tools that change nothing. Other coding agents are refused. Safe while a build runs: it doesn't interrupt it. Returns the answer, how it was answered and its cost, and keeps it in the project (`rebuild_questions_list`).",
            beginner: "Asks about what was made, without changing it.",
            toolset: Toolset::Projects,
            effects: Effects::LAUNCHES_PROCESS.union(Effects::ACCESSES_NETWORK),
            requires: Requires::Nothing,
            input_schema: schema_of::<QuestionParams>,
            run: rebuild_question_ask,
        },
        ToolSpec {
            id: "rebuild_questions_list",
            title: "List questions about a build",
            summary: "The questions asked about a project's build and their answers, oldest first (the newest 200).",
            beginner: "Shows what you've asked about this project.",
            toolset: Toolset::Projects,
            effects: Effects::READ_ONLY,
            requires: Requires::Nothing,
            input_schema: schema_of::<ProjectParams>,
            run: rebuild_questions_list,
        },
        ToolSpec {
            id: "rebuild_check",
            title: "Check my build",
            summary: "For the builder: test the project's current build against the original now, with the same checks Craft Anything runs after each turn (a website's likeness at each width with the padding, colour, font and layout differences; an app's or game's picture against the original's; whether it builds; the mechanics sheet; credits). The project's running build answers it, so the builder can fix what it lists and check again before replying. Not counted as a round. project defaults to the project this tool server works for.",
            beginner: "Lets the builder test its work against the original while it builds.",
            toolset: Toolset::Core,
            effects: Effects::WRITES_FILESYSTEM,
            requires: Requires::Nothing,
            input_schema: schema_of::<CheckParams>,
            run: rebuild_check,
        },
        ToolSpec {
            id: "project_inventory_rebuild",
            title: "Rebuild a group of assets",
            summary: "Rebuild a category of a project's assets (or items by id) the cheapest way (see project_inventory_rebuild_plan; `methods` chooses per kind). Writes the list with each item's method to <project>/rebuild/<category>-<time>.json, marks those items needs_replacing with their rebuild, and hands the builder a brief (free methods only, never paid services; mark each item replaced when it's in the build): as a note to a running build, or a follow-up build. Kinds whose method isn't set up yet, and kept ones, are skipped and listed. send: false only writes and marks, and returns the brief. It runs a coding agent, so it needs consent.",
            beginner: "Remakes a group of assets, such as the HUD, as cheaply as possible.",
            toolset: Toolset::Projects,
            effects: Effects::LAUNCHES_PROCESS.union(Effects::EXECUTES_FOREIGN_CODE).union(Effects::WRITES_FILESYSTEM).union(Effects::ACCESSES_NETWORK),
            requires: Requires::Nothing,
            input_schema: schema_of::<AssetRebuildParams>,
            run: project_inventory_rebuild,
        },
    ]
}

fn d_true() -> bool {
    true
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AssetRebuildParams {
    /// The project's folder.
    pub project: PathBuf,
    /// The category to rebuild (characters, enemies, weapons, items, machines, hud, environment, effects, music, unsorted).
    #[serde(default)]
    pub category: Option<craft_anything_engine::asset_categories::Category>,
    /// Or these items (ids from project_inventory_list).
    #[serde(default)]
    pub ids: Vec<String>,
    /// A method per kind (kind id → method id); the rest get their default.
    #[serde(default)]
    pub methods: std::collections::BTreeMap<String, String>,
    /// Hand it to the builder now (default true); false only writes the list and marks the items.
    #[serde(default = "d_true")]
    pub send: bool,
}

fn project_inventory_rebuild(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    use craft_anything_engine::tools::inventory_tools::categories::{PlanParams, start_plan};
    let p: AssetRebuildParams = parse_params(params)?;
    let plan = PlanParams { project: p.project, category: p.category, ids: p.ids, methods: p.methods };
    let (ws, started, mut out) = start_plan(ctx, &plan)?;
    if !p.send || started.items == 0 {
        out["sent"] = json!(false);
        return Ok(ToolOutput::Value(out));
    }
    // A running build reads it at its next step; otherwise it starts a follow-up.
    let note = {
        let runs = runs(ctx);
        let mut set = runs.0.lock().unwrap_or_else(PoisonError::into_inner);
        refresh(ctx, &mut set);
        set.get(&ws.root).filter(|r| r.is_running()).map(|r| r.add_note(started.brief.clone(), started.brief.clone(), Vec::new()))
    };
    // The list and the marks are written either way; a builder that can't start is said, not hidden.
    out["sent"] = match note {
        Some(id) => json!({"note": id}),
        None => match rebuild_start(ctx, json!({"project": ws.root, "request": started.brief})) {
            Ok(o) => o.to_json()["result"].clone(),
            Err(e) => json!({"error": e.message, "remediation": e.remediation}),
        },
    };
    Ok(ToolOutput::Value(out))
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StartParams {
    /// What to make (a new project), or the follow-up message (with `project`).
    pub request: String,
    /// For a new project: a web address, an absolute path to an app, game or file, or `-` for none.
    #[serde(default)]
    pub target: Option<String>,
    /// Continue this project folder instead of starting a new one.
    #[serde(default)]
    pub project: Option<PathBuf>,
    /// With `project`: carry on where its last run stopped (paused, or the app or the computer
    /// stopped) instead of sending `request` as a follow-up.
    #[serde(default)]
    pub resume: bool,
    /// Where a new project's folder goes (default: the projects folder from settings).
    #[serde(default)]
    pub projects_dir: Option<PathBuf>,
    /// Start the new project in this folder itself instead (it's added to the project list). If
    /// it already has files, `existing` says what they are.
    #[serde(default)]
    pub project_folder: Option<PathBuf>,
    /// With `project_folder` that has files: `edit` (they're the app, edited in place) or
    /// `start_from` (material to start from; the app is built in app/ beside them).
    #[serde(default)]
    pub existing: Option<craft_anything_engine::workspace::ExistingFiles>,
    /// What to build with: an engine (unity, unreal, godot, rust) or a stack's full name.
    #[serde(default)]
    pub build_with: Option<String>,
    /// For a new game: the builder may generate pictures and 3D models (paid from the person's
    /// asset provider key, within the project's budget). Default false; change it later with
    /// project_assets_allow.
    #[serde(default)]
    pub generate_assets: bool,
    /// The coding agent to use (an enabled backend name), kept with a new project for its later
    /// changes; default: the one set up for building.
    #[serde(default)]
    pub backend: Option<String>,
    /// Refinement rounds after the first build (default 2).
    #[serde(default)]
    pub rounds: Option<u32>,
    /// Pictures of the original (absolute paths): the look the person expects. They're copied
    /// into the project's reference/ folder for the builder.
    #[serde(default)]
    pub pictures: Vec<PathBuf>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckParams {
    /// The project's folder; default: the project this tool server was started for.
    #[serde(default)]
    pub project: Option<PathBuf>,
    /// Longest wait for the result, in minutes (1..=60, default 20): a website with several pages
    /// or a game's engine build takes a few minutes.
    #[serde(default = "d_wait_minutes")]
    pub wait_minutes: u64,
}

fn d_wait_minutes() -> u64 {
    20
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectParams {
    /// The project's folder.
    pub project: PathBuf,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActivityParams {
    /// The project's folder.
    pub project: PathBuf,
    /// How many lines to return (200 by default, at most 2000).
    #[serde(default)]
    pub limit: Option<usize>,
    /// How many lines back from the newest to end at (0 by default).
    #[serde(default)]
    pub offset: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NoteParams {
    /// The project's folder.
    pub project: PathBuf,
    /// What to tell the builder.
    pub text: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WithdrawParams {
    /// The project's folder.
    pub project: PathBuf,
    /// The note's id, as rebuild_note returned it.
    pub id: u64,
}

/// Builds started by tools in this session, by project folder.
struct Runs(Arc<Mutex<RunSet>>);

/// How often queued builds are checked for a free place between tool calls.
const QUEUE_TICK: Duration = Duration::from_secs(2);

fn runs(ctx: &mut Ctx<'_>) -> Arc<Runs> {
    match ctx.session.ext.get::<Runs>() {
        Some(r) => r,
        None => {
            let r = Arc::new(Runs(Arc::new(Mutex::new(RunSet::new(ctx.session.settings.build.parallel)))));
            ctx.session.ext.insert(r.clone());
            r
        }
    }
}

/// The engine the rebuild tools were registered on.
fn engine(ctx: &Ctx<'_>) -> Option<Arc<Engine>> {
    ctx.session.ext.get::<EngineRef>().and_then(|r| r.0.upgrade())
}

/// Bring every run up to date and start queued ones in free places. Events are kept in each
/// project's feed by the run itself, so they're dropped here.
fn refresh(ctx: &Ctx<'_>, set: &mut RunSet) {
    set.set_limit(ctx.session.settings.build.parallel);
    if let Some(engine) = engine(ctx) {
        let hub = AiHub::with_stores(engine, ctx.session.config_dir().join("config.toml"), crate::KeyStore::system());
        let _ = set.poll(&hub);
    }
}

fn open(project: &Path) -> Result<Workspace, ToolError> {
    Workspace::open(project).map_err(|e| {
        ToolError::new(ErrorCode::NotFound, format!("{} isn't a project folder: {}", project.display(), e.message))
            .with_remediation("Pass a folder from project_list (it holds craft.json).")
    })
}

/// The question tool's id.
pub const QUESTION_TOOL: &str = "rebuild_question_ask";

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionParams {
    /// The project's folder.
    pub project: PathBuf,
    /// The question, in plain words.
    pub question: String,
}

fn rebuild_question_ask(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let p: QuestionParams = parse_params(params)?;
    let engine = engine(ctx).ok_or_else(|| {
        ToolError::new(ErrorCode::FeatureMissing, "Questions about a build aren't available in this engine.")
            .with_remediation("Use the app or `craft-anything-cli`.")
    })?;
    let ws = open(&p.project)?;
    let after = craft_anything_engine::feed::load(&ws).len();
    ctx.report(None, "Asking…");
    // The session is this call's while it runs: the question gets its settings from here.
    let mut settings = ctx.session.settings.clone();
    settings.deep_mode = !ctx.session.clean_room();
    let answered =
        super::question::ask(&AiHub::new(engine), &settings, Some(crate::failure_log::dir_in(&ctx.session.config_dir())), &ws, &p.question, &ctx.cancel)?;
    let question = p.question.trim();
    let from_session = answered.from == super::question::AnsweredFrom::BuildersSession;
    let asked = AskedQuestion {
        deep: settings.deep_mode,
        ..AskedQuestion::new(question.to_string(), answered.answer.clone(), from_session, answered.backend.clone(), answered.cost_usd, after)
    };
    // The replacement notice is for this caller; the saved question keeps the AI that answered.
    // An answer that couldn't be kept is still the answer.
    let not_kept = questions::add(&ws, asked).err().map(|e| e.message);
    Ok(ToolOutput::Value(json!({"project": ws.root, "question": question, "answered": answered, "not_kept": not_kept})))
}

fn rebuild_questions_list(_ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let p: ProjectParams = parse_params(params)?;
    let questions = questions::load(&open(&p.project)?);
    Ok(ToolOutput::Value(json!({"count": questions.len(), "questions": questions})))
}

fn rebuild_start(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let p: StartParams = parse_params(params)?;
    let engine = engine(ctx).ok_or_else(|| {
        ToolError::new(ErrorCode::FeatureMissing, "Building isn't available in this engine.").with_remediation("Use the app or `craft-anything-cli rebuild`.")
    })?;
    let runs = runs(ctx);
    let cfg = crate::AiConfig::from_settings(&ctx.session.settings);
    // Refuse an unknown or switched-off name before opening or making a project or adding pictures:
    // a refused start must not leave anything behind.
    if let Some(name) = p.backend.as_deref() {
        builder_backend(&cfg, Some(name))?;
    }
    let (ws, message) = match &p.project {
        Some(dir) => {
            if p.build_with.is_some() {
                return Err(ToolError::invalid("build_with is for new projects; a project keeps the stack it was built with."));
            }
            if p.generate_assets {
                return Err(ToolError::invalid("generate_assets is for new projects.").with_remediation("Use project_assets_allow for an existing project."));
            }
            let ws = open(dir)?;
            ctx.session.remember_project(&ws.root)?;
            (ws, (!p.resume).then(|| p.request.clone()))
        }
        None if p.resume => return Err(ToolError::invalid("resume is for an existing project.").with_remediation("Pass its folder as `project`.")),
        None => {
            let target = p.target.as_deref().unwrap_or("-");
            let place = match (&p.project_folder, &p.projects_dir) {
                (Some(_), Some(_)) => return Err(ToolError::invalid("Pass projects_dir or project_folder, not both.")),
                (Some(f), None) => project::Place::At(f, p.existing),
                (None, Some(d)) => project::Place::In(d),
                (None, None) => project::Place::Projects,
            };
            let ws = project::new_project(&ctx.session.settings, target, &p.request, place, p.build_with.as_deref(), p.generate_assets, p.backend.as_deref())?;
            if p.project_folder.is_some() || matches!(place, project::Place::At(..)) {
                ctx.session.remember_project(&ws.root)?;
            }
            (ws, None)
        }
    };
    if !p.pictures.is_empty() {
        ws.add_references(&p.pictures)?;
    }
    // A headless start can't show the app's toast: say which AI builds when the saved one is gone.
    let builder_notice =
        p.project.as_ref().and_then(|_| super::builder_replacement(&cfg, &ws, p.backend.as_deref()).map(|r| super::replaced_builder_notice(&r.saved, &r.used)));
    let mut set = runs.0.lock().unwrap_or_else(PoisonError::into_inner);
    refresh(ctx, &mut set);
    if set.is_busy(&ws.root) {
        return Err(ToolError::new(ErrorCode::BadInput, "This project is already building.")
            .with_remediation("Add to it with rebuild_note, or stop it with rebuild_stop."));
    }
    let root = ws.root.clone();
    let opts = RunOptions {
        backend: p.backend,
        shown: message.clone(),
        message,
        continuing: p.resume,
        max_rounds: p.rounds.unwrap_or(2),
        beginner: false,
        clean_room: Some(ctx.session.clean_room()),
        ..RunOptions::default()
    };
    // A tool runs with the session locked, so the hub reads the saved file: use this session's
    // config file (the person's own in the app), not the machine's default one.
    let hub = AiHub::with_stores(engine, ctx.session.config_dir().join("config.toml"), crate::KeyStore::system());
    let mut out = match set.start(&hub, ws, opts)? {
        Started::Running => json!({"project": root.display().to_string(), "started": true}),
        Started::Queued { position } => {
            // It starts when a place comes free, whether or not another tool is called.
            drop(set);
            runs::keep_queue_moving(&runs.0, &hub.engine, ctx.session.config_dir().join("config.toml"), QUEUE_TICK);
            json!({"project": root.display().to_string(), "started": false, "queued": position})
        }
    };
    if let Some(notice) = builder_notice {
        out["builder_notice"] = json!(notice);
    }
    Ok(ToolOutput::Value(out))
}

/// A run that only checks the project's build ([`RunOptions::check_only`]).
fn rebuild_check_now(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let p: ProjectParams = parse_params(params)?;
    let engine = engine(ctx).ok_or_else(|| {
        ToolError::new(ErrorCode::FeatureMissing, "Checking a build isn't available in this engine.").with_remediation("Use the app or the CLI.")
    })?;
    let runs = runs(ctx);
    let ws = open(&p.project)?;
    let mut set = runs.0.lock().unwrap_or_else(PoisonError::into_inner);
    refresh(ctx, &mut set);
    if set.is_busy(&ws.root) {
        return Err(ToolError::new(ErrorCode::BadInput, "This project is already building.")
            .with_remediation("Its checks run when the builder's turn ends. To check now, stop it with rebuild_stop first."));
    }
    let root = ws.root.clone();
    let opts =
        RunOptions { continuing: true, check_only: true, max_rounds: 0, beginner: false, clean_room: Some(ctx.session.clean_room()), ..RunOptions::default() };
    let hub = AiHub::with_stores(engine, ctx.session.config_dir().join("config.toml"), crate::KeyStore::system());
    let out = match set.start(&hub, ws, opts)? {
        Started::Running => json!({"project": root.display().to_string(), "started": true}),
        Started::Queued { position } => {
            drop(set);
            runs::keep_queue_moving(&runs.0, &hub.engine, ctx.session.config_dir().join("config.toml"), QUEUE_TICK);
            json!({"project": root.display().to_string(), "started": false, "queued": position})
        }
    };
    Ok(ToolOutput::Value(out))
}

fn rebuild_status(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let p: ProjectParams = parse_params(params)?;
    let ws = open(&p.project)?;
    let runs = runs(ctx);
    let mut set = runs.0.lock().unwrap_or_else(PoisonError::into_inner);
    refresh(ctx, &mut set);
    let m = &ws.manifest;
    // What the app's status row shows for a running build, so a CLI watcher sees the same.
    let run = set.get(&ws.root).filter(|r| r.is_running());
    Ok(ToolOutput::Value(json!({
        "project": ws.root.display().to_string(),
        "running": run.is_some(),
        "queued": set.position(&ws.root),
        "status": m.status,
        "status_label": m.status.label(),
        "note": m.note,
        "last_check": m.checks.last(),
        "waiting": Inbox::new(ws.inbox_dir()).pending().len(),
        "started_secs_ago": run.map(|r| r.started().elapsed().as_secs()),
        "quiet_secs": run.map(|r| r.quiet_for().as_secs()),
        "now": run.and_then(|r| r.activity_now().words()),
        "last_said": run.and_then(RebuildRun::last_said),
    })))
}

fn rebuild_activity_show(_ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let p: ActivityParams = parse_params(params)?;
    let ws = open(&p.project)?;
    let limit = p.limit.unwrap_or(200).min(2000);
    let (lines, total) = super::activity::read_log(&ws.root, p.offset.unwrap_or(0), limit)?;
    Ok(ToolOutput::Value(json!({
        "project": ws.root.display().to_string(),
        "log": ws.root.join(craft_anything_engine::workspace::ACTIVITY_LOG).display().to_string(),
        "lines": lines,
        "total_lines_read": total,
    })))
}

fn rebuild_note(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let p: NoteParams = parse_params(params)?;
    let ws = open(&p.project)?;
    let runs = runs(ctx);
    let mut set = runs.0.lock().unwrap_or_else(PoisonError::into_inner);
    refresh(ctx, &mut set);
    match set.get(&ws.root).filter(|r| r.is_running()) {
        Some(run) => {
            let id = run.add_note(p.text.clone(), p.text, Vec::new());
            Ok(ToolOutput::Value(json!({"id": id, "queued": run.queued()})))
        }
        None => Err(ToolError::new(ErrorCode::BadInput, "No build is running for this project in this session.")
            .with_remediation("Start a follow-up with rebuild_start (pass project and your message).")),
    }
}

fn rebuild_note_withdraw(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let p: WithdrawParams = parse_params(params)?;
    let ws = open(&p.project)?;
    let runs = runs(ctx);
    let mut set = runs.0.lock().unwrap_or_else(PoisonError::into_inner);
    refresh(ctx, &mut set);
    match set.get(&ws.root).and_then(|r| r.unsend(p.id)) {
        Some(note) => Ok(ToolOutput::Value(json!({"withdrawn": note.text}))),
        None => Err(ToolError::new(ErrorCode::BadInput, format!("Note {} isn't waiting: the agent has read it, or it isn't from this session.", p.id))),
    }
}

fn rebuild_stop(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let p: ProjectParams = parse_params(params)?;
    let ws = open(&p.project)?;
    let runs = runs(ctx);
    let mut set = runs.0.lock().unwrap_or_else(PoisonError::into_inner);
    refresh(ctx, &mut set);
    match set.stop(&ws.root) {
        Stopped::Stopping => Ok(ToolOutput::Value(json!({"stopping": ws.root.display().to_string()}))),
        Stopped::Unqueued => Ok(ToolOutput::Value(json!({"unqueued": ws.root.display().to_string()}))),
        Stopped::Nothing => Err(ToolError::new(ErrorCode::BadInput, "No build is running for this project in this session.")),
    }
}

/// Ask the project's running build to check the current build (see [`super::self_check`]) and
/// wait for what it found.
fn rebuild_check(ctx: &mut Ctx<'_>, params: Value) -> Result<ToolOutput, ToolError> {
    let p: CheckParams = parse_params(params)?;
    if !(1..=60).contains(&p.wait_minutes) {
        return Err(ToolError::invalid("wait_minutes must be 1..=60"));
    }
    // The tool server is started with the project's inbox (<project>/inbox).
    let project = p
        .project
        .or_else(|| ctx.session.ext.get::<InboxDir>().and_then(|d| d.0.parent().map(Path::to_path_buf)))
        .ok_or_else(|| ToolError::invalid("No project given.").with_remediation("Pass project: the project's folder (it holds craft.json)."))?;
    let ws = open(&project)?;
    if self_check::building_here(&ws.root) {
        return Err(ToolError::new(ErrorCode::FeatureMissing, "This builder can't check its own work while it builds.")
            .with_remediation("Finish the turn: Craft Anything checks the build after it."));
    }
    let asks = Inbox::new(self_check::asks_dir(&ws.root));
    let wait = Duration::from_secs(p.wait_minutes * 60);
    // The run reads the wait and stops the check before it ends, so an answer comes either way.
    let detail = self_check::CheckAsk::detail(wait);
    let id = asks.post(craft_anything_engine::inbox::RequestKind::Question, "Check the build", &detail, Vec::new(), false, None)?;
    ctx.report(None, "Checking the build against the original…");
    let cancel = ctx.cancel.clone();
    match asks.wait(&id, wait, &|| cancel.is_cancelled()) {
        Some(a) => Ok(ToolOutput::Value(json!({"project": ws.root.display().to_string(), "report": a.text.unwrap_or_default()}))),
        None if ctx.cancel.is_cancelled() => Err(ToolError::new(ErrorCode::Cancelled, "Stopped.")),
        None => Err(no_check_answer(&ws.root, p.wait_minutes)),
    }
}

/// Why `rebuild_check` got no answer: the project's status says whether a run is working on it
/// (then the run didn't answer in time) or nothing is building it at all.
fn no_check_answer(root: &Path, wait_minutes: u64) -> ToolError {
    let status = Workspace::open(root).map(|w| w.manifest.status).unwrap_or_default();
    if matches!(status, Status::Inspecting | Status::Building | Status::Checking) {
        ToolError::new(ErrorCode::TimedOut, format!("No answer after {wait_minutes} minutes: the build's run didn't finish the check in time."))
            .with_remediation("Finish the turn: Craft Anything checks the build after it. For a long build, pass a bigger wait_minutes next time.")
    } else {
        ToolError::new(
            ErrorCode::NotFound,
            format!("No answer: no build is running for this project (its status is {}), so nothing can check it now.", status.label()),
        )
        .with_remediation("Use this while Craft Anything is building the project; otherwise build it and finish the turn, and it is checked after.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use craft_anything_engine::{CallOptions, Caller, Consent, Engine};

    #[test]
    fn a_question_through_the_engine_comes_back_instead_of_waiting_for_the_session() {
        // The tool runs while the call holds the session; reading it again would wait forever.
        let mut s = craft_anything_engine::testing::session();
        s.settings.ai =
            toml::from_str("[[backends]]\nname = \"Agent\"\nkind = \"agent_cli\"\nmodel = \"claude-code\"\ncommand = \"craft-anything-no-such-agent\"\n")
                .unwrap();
        let e = std::sync::Arc::new(Engine::shared(s, specs()));
        let d = tempfile::tempdir().unwrap();
        let ws = Workspace::create(d.path(), "Snake", "A snake game", TargetRef::None).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let e2 = e.clone();
        std::thread::spawn(move || {
            let r = e2.call(QUESTION_TOOL, json!({"project": ws.root, "question": "where are the files?"}), CallOptions::new(Caller::Cli));
            let _ = tx.send(r.map(|o| o.to_json()));
        });
        let r = rx.recv_timeout(std::time::Duration::from_secs(60)).expect("the question came back");
        // No such agent here: an error that says so, not a hang.
        assert!(r.is_err(), "{r:?}");
    }

    #[test]
    fn rebuild_tools_are_registered_from_the_ai_layer_and_check_their_input() {
        let d = tempfile::tempdir().unwrap();
        let e = Engine::shared(craft_anything_engine::testing::session(), specs());
        let ok = || CallOptions::new(Caller::Cli).with_consent(Consent::Granted);
        for id in ["rebuild_start", "rebuild_status", "rebuild_note", "rebuild_note_withdraw", "rebuild_stop", "rebuild_check"] {
            assert!(e.registry().get(id).is_some(), "{id}");
        }
        // Starting runs a coding agent: refused without consent, before anything is created.
        let p = json!({"request": "A page", "target": "-", "projects_dir": d.path()});
        assert!(e.call("rebuild_start", p, CallOptions::new(Caller::Mcp)).is_err());
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 0, "nothing created without consent");
        // A bad target or stack is refused before a project is made.
        assert!(e.call("rebuild_start", json!({"request": "x", "target": "not a path", "projects_dir": d.path()}), ok()).is_err());
        assert!(e.call("rebuild_start", json!({"request": "x", "build_with": "cobol", "projects_dir": d.path()}), ok()).is_err());
        // Status, notes and stop on a project with nothing running.
        let ws = Workspace::create(d.path(), "Snake", "A snake game", TargetRef::None).unwrap();
        // A project's activity reads through the tool, none yet being no lines, not an error.
        let shown = e.call("rebuild_activity_show", json!({"project": ws.root}), ok()).unwrap().to_json();
        assert_eq!(shown["result"]["lines"], json!([]), "{shown}");
        std::fs::write(ws.root.join(craft_anything_engine::workspace::ACTIVITY_LOG), "one\ntwo\nthree\n").unwrap();
        let shown = e.call("rebuild_activity_show", json!({"project": ws.root, "limit": 1, "offset": 1}), ok()).unwrap().to_json();
        assert_eq!(shown["result"]["lines"], json!(["two"]), "{shown}");
        let st = e.call("rebuild_status", json!({"project": ws.root}), ok()).unwrap().to_json();
        assert_eq!(st["result"]["running"], false);
        assert_eq!(st["result"]["queued"], Value::Null);
        // Nothing running: the live fields are there and empty, so a watcher can tell "quiet"
        // from "not running".
        for field in ["started_secs_ago", "quiet_secs", "now", "last_said"] {
            assert!(st["result"].get(field).is_some(), "{field} missing: {st}");
            assert_eq!(st["result"][field], Value::Null, "{field}: {st}");
        }
        assert!(e.call("rebuild_note", json!({"project": ws.root, "text": "blue"}), ok()).is_err());
        assert!(e.call("rebuild_note_withdraw", json!({"project": ws.root, "id": 1}), ok()).is_err());
        assert!(e.call("rebuild_stop", json!({"project": ws.root}), ok()).is_err());
    }

    #[test]
    fn a_disabled_or_unknown_backend_is_refused_before_the_tool_makes_a_project() {
        let d = tempfile::tempdir().unwrap();
        let mut s = craft_anything_engine::testing::session();
        s.settings.ai = toml::from_str("[[backends]]\nname = \"Off\"\nkind = \"anthropic\"\nmodel = \"claude-test\"\nenabled = false\n").unwrap();
        let e = Engine::shared(s, specs());
        let ok = || CallOptions::new(Caller::Cli).with_consent(Consent::Granted);
        for (backend, expected) in [("Off", "turned off"), ("No Such AI", "No AI backend named")] {
            let p = json!({"request": "A page", "target": "-", "projects_dir": d.path(), "backend": backend});
            let err = e.call("rebuild_start", p, ok()).unwrap_err();
            assert!(err.message.contains(expected), "{backend}: {err:?}");
            assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 0, "{backend}: a refused start must not make a project");
        }
    }

    #[test]
    fn an_asset_group_is_rebuilt_through_the_projects_builder() {
        use craft_anything_engine::inventory::{self, INVENTORY_VERSION, Inventory, Item, Kind, Source, Status};
        let d = tempfile::tempdir().unwrap();
        let mut s = craft_anything_engine::testing::session();
        s.settings.ai = toml::from_str(
            "[[backends]]
name = \"Agent\"
kind = \"agent_cli\"
model = \"claude-code\"
",
        )
        .unwrap();
        s.settings.save_to(&s.config_dir().join("config.toml")).unwrap();
        let fake = crate::rebuild::StandIns::new();
        s.ext.insert(Arc::new(Runs(Arc::new(Mutex::new(RunSet::new(1).with_starter(fake.starter()))))));
        let e = Engine::shared(s, specs());
        let ws = Workspace::create(d.path(), "Game", "A game", TargetRef::None).unwrap();
        let items = ["Icons_Back", "HexButton", "Glyphid_Grunt"].iter().map(|n| Item::new(Source::Build, n, Kind::Texture, *n, Status::Placeholder)).collect();
        let mut inv = Inventory { version: INVENTORY_VERSION, items, ..Default::default() };
        craft_anything_engine::asset_categories::apply(&mut inv);
        inventory::save(&ws.root, &inv).unwrap();
        let ok = || CallOptions::new(Caller::Cli).with_consent(Consent::Granted);
        // Running a coding agent needs consent.
        assert!(e.call("project_inventory_rebuild", json!({"project": ws.root, "category": "hud"}), CallOptions::new(Caller::Mcp)).is_err());
        let r = e.call("project_inventory_rebuild", json!({"project": ws.root, "category": "hud"}), ok()).unwrap().to_json()["result"].clone();
        assert_eq!(r["started"], 2, "{r}");
        assert_eq!(r["sent"]["started"], true, "a follow-up build started: {r}");
        assert_eq!(fake.started(), std::slice::from_ref(&ws.root));
        assert!(std::path::Path::new(r["list"].as_str().unwrap()).is_file());
        let inv = inventory::load(&ws.root).unwrap();
        assert_eq!(inv.items.iter().filter(|i| i.status == Status::NeedsReplacing && i.rebuild.is_some()).count(), 2);
        // While it builds, another group goes to it as a note.
        let r = e.call("project_inventory_rebuild", json!({"project": ws.root, "ids": [inv.items[2].id]}), ok()).unwrap().to_json()["result"].clone();
        assert!(r["sent"]["note"].is_u64(), "{r}");
        // send: false only writes and marks.
        let r = e.call("project_inventory_rebuild", json!({"project": ws.root, "category": "hud", "send": false}), ok()).unwrap().to_json()["result"].clone();
        assert_eq!(r["sent"], false);
        assert!(r["brief"].as_str().unwrap().contains("project_inventory_mark"));
    }

    /// A CLI watcher sees what the app's status row shows: how long the build has gone, how long
    /// it has been quiet, and what it's on in words.
    #[test]
    fn rebuild_status_says_how_long_a_build_has_gone_and_been_quiet_and_what_it_is_on() {
        use crate::rebuild::{RunEvent, StandIns, Step};
        let d = tempfile::tempdir().unwrap();
        let mut s = craft_anything_engine::testing::session();
        s.settings.ai = toml::from_str("[[backends]]\nname = \"Agent\"\nkind = \"agent_cli\"\nmodel = \"claude-code\"\n").unwrap();
        s.settings.save_to(&s.config_dir().join("config.toml")).unwrap();
        // The session's builds are stand-ins, driven from here.
        let fake = StandIns::new();
        s.ext.insert(Arc::new(Runs(Arc::new(Mutex::new(RunSet::new(1).with_starter(fake.starter()))))));
        let e = Engine::shared(s, specs());
        let ws = Workspace::create(d.path(), "Notes", "A notes app", TargetRef::None).unwrap();
        let ok = || CallOptions::new(Caller::Cli).with_consent(Consent::Granted);
        let started = e.call("rebuild_start", json!({"project": ws.root, "request": "make it blue"}), ok()).unwrap().to_json();
        assert_eq!(started["result"]["started"], true, "{started}");
        assert!(fake.send(&ws.root, RunEvent::Step { step: Step::Build, text: "Building".into() }));
        assert!(fake.send(
            &ws.root,
            RunEvent::Agent(crate::AgentEvent::ToolStarted { call_id: "1".into(), tool: "Bash".into(), input: json!({"command": "cargo test --quiet"}) })
        ));
        let st = e.call("rebuild_status", json!({"project": ws.root}), ok()).unwrap().to_json();
        assert_eq!(st["result"]["running"], true, "{st}");
        assert!(st["result"]["quiet_secs"].as_u64().is_some_and(|q| q < 60), "{st}");
        assert!(st["result"]["now"].as_str().is_some_and(|n| n.starts_with("Now: Running cargo test --quiet (")), "{st}");
        assert_eq!(st["result"]["last_said"], Value::Null, "{st}");
        // Six quiet minutes later (the run is made to look older; the test doesn't wait).
        assert!(fake.backdate(&ws.root, Duration::from_secs(6 * 60)));
        let st = e.call("rebuild_status", json!({"project": ws.root}), ok()).unwrap().to_json();
        assert!(st["result"]["quiet_secs"].as_u64().is_some_and(|q| q >= 360), "{st}");
        assert!(st["result"]["started_secs_ago"].as_u64().is_some_and(|q| q >= 360), "{st}");
        // Said, then nothing: the last line it said is there for whoever is watching.
        assert!(fake.send(&ws.root, RunEvent::Agent(crate::AgentEvent::TextDelta { text: "Looking at the tests now.\n".into() })));
        let st = e.call("rebuild_status", json!({"project": ws.root}), ok()).unwrap().to_json();
        assert_eq!(st["result"]["last_said"], "Looking at the tests now.", "{st}");
        assert!(st["result"]["quiet_secs"].as_u64().is_some_and(|q| q < 60), "an event ends the quiet: {st}");
        assert!(fake.finish(&ws.root));
        let st = e.call("rebuild_status", json!({"project": ws.root}), ok()).unwrap().to_json();
        assert_eq!(st["result"]["running"], false, "{st}");
        assert_eq!(st["result"]["quiet_secs"], Value::Null, "{st}");
    }

    /// #1102: a tool holds the session while it runs, so starting a build on an API backend must not
    /// ask for it again; over MCP the call waited on itself until the deadlock was broken.
    #[test]
    fn rebuild_start_on_an_api_backend_does_not_wait_on_its_own_session() {
        let d = tempfile::tempdir().unwrap();
        let mut s = craft_anything_engine::testing::session();
        let mut ai = crate::AiConfig::default();
        ai.upsert(crate::BackendEntry {
            config: crate::BackendConfig {
                name: "Local".into(),
                kind: crate::BackendKind::OpenaiCompatible,
                model: "qwen3:8b".into(),
                // Nothing listens there: the run only has to start, not to get an answer.
                base_url: Some("http://127.0.0.1:9/v1".into()),
                ssh_host: None,
                enabled: true,
            },
            options: Default::default(),
        });
        s.settings.ai = ai.to_table().unwrap();
        s.settings.save_to(&s.config_dir().join("config.toml")).unwrap();
        let e = Engine::shared(s, specs());
        let ws = Workspace::create(d.path(), "Notes", "A notes app", TargetRef::None).unwrap();
        let ok = || CallOptions::new(Caller::Mcp).with_consent(Consent::Granted);
        let (tx, rx) = std::sync::mpsc::channel();
        let (call, root) = (e.clone(), ws.root.clone());
        std::thread::spawn(move || {
            let _ = tx.send(call.call("rebuild_start", json!({"project": root, "request": "make it blue", "rounds": 0, "backend": "Local"}), ok()));
        });
        let started = rx.recv_timeout(std::time::Duration::from_secs(60)).expect("rebuild_start returned instead of waiting on its own session");
        assert!(started.is_ok(), "{:?}", started.err());
        let _ = e.call("rebuild_stop", json!({"project": ws.root}), ok());
    }

    /// A saved builder that is off is named in the start result, so MCP callers see the
    /// replacement too.
    #[cfg(unix)]
    #[test]
    fn rebuild_start_names_a_replaced_saved_builder() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let script = d.path().join("claude");
        std::fs::write(
            &script,
            "#!/bin/sh\ncat >/dev/null\nprintf '%s\\n' '{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"Made it blue.\"}]}}'\nprintf '%s\\n' '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"num_turns\":1}'\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut s = craft_anything_engine::testing::session();
        let mut ai = crate::AiConfig::default();
        ai.upsert(crate::BackendEntry {
            config: crate::BackendConfig {
                name: "Off".into(),
                kind: crate::BackendKind::Anthropic,
                model: "claude-test".into(),
                base_url: None,
                ssh_host: None,
                enabled: false,
            },
            options: Default::default(),
        });
        ai.upsert(crate::BackendEntry {
            config: crate::BackendConfig {
                name: "Claude".into(),
                kind: crate::BackendKind::AgentCli,
                model: "claude-code".into(),
                base_url: None,
                ssh_host: None,
                enabled: true,
            },
            options: crate::BackendOptions { command: Some(script.display().to_string()), ..Default::default() },
        });
        s.settings.ai = ai.to_table().unwrap();
        // The tool runs with the session locked; save the same settings where its hub reads them.
        s.settings.save_to(&s.config_dir().join("config.toml")).unwrap();
        let e = Engine::shared(s, specs());
        let mut ws = Workspace::create(d.path(), "Notes", "A notes app", TargetRef::None).unwrap();
        ws.set_person(|m| m.builder = Some("Off".into())).unwrap();
        let ok = || CallOptions::new(Caller::Cli).with_consent(Consent::Granted);
        let out = e.call("rebuild_start", json!({"project": ws.root, "request": "make it blue", "rounds": 0}), ok()).unwrap().to_json();
        let notice = out["result"]["builder_notice"].as_str().unwrap_or_default();
        assert!(notice.contains("Off") && notice.contains("Claude"), "{out}");
        let _ = e.call("rebuild_stop", json!({"project": ws.root}), ok());
    }

    /// "Check it now" headless: it needs consent (it runs the project's own build), a real
    /// project and a builder to resolve, and says plainly when it can't start.
    #[test]
    fn checking_a_build_now_needs_consent_a_project_and_a_builder() {
        let d = tempfile::tempdir().unwrap();
        let e = Engine::shared(craft_anything_engine::testing::session(), specs());
        let ws = Workspace::create(d.path(), "Notes", "A notes app", TargetRef::None).unwrap();
        let asked = e.call("rebuild_check_now", json!({"project": ws.root}), CallOptions::new(Caller::Mcp));
        assert!(asked.is_err(), "it runs the project's build, so it asks first");
        let ok = || CallOptions::new(Caller::Cli).with_consent(Consent::Granted);
        assert!(e.call("rebuild_check_now", json!({"project": d.path().join("nope")}), ok()).is_err(), "not a project");
        // No AI is set up in this session: nothing starts, and the error says why.
        let err = e.call("rebuild_check_now", json!({"project": ws.root}), ok()).unwrap_err();
        assert!(!err.message.is_empty(), "{err:?}");
        assert_eq!(e.call("rebuild_status", json!({"project": ws.root}), ok()).unwrap().to_json()["result"]["running"], json!(false));
    }

    #[test]
    fn the_builder_asks_the_running_build_for_a_check_and_gets_its_report() {
        let d = tempfile::tempdir().unwrap();
        let e = Engine::shared(craft_anything_engine::testing::session(), specs());
        let ok = || CallOptions::new(Caller::Mcp);
        let ws = Workspace::create(d.path(), "Snake", "A snake game", TargetRef::None).unwrap();
        assert!(e.call("rebuild_check", json!({"project": ws.root, "wait_minutes": 0}), ok()).is_err(), "wait is checked");
        assert!(e.call("rebuild_check", json!({"project": d.path().join("nope")}), ok()).is_err(), "not a project");
        assert!(e.call("rebuild_check", json!({"wait_minutes": 1}), ok()).is_err(), "no project and no inbox to infer it from");
        // A stand-in for the run: answer the first check request with a report.
        let asks = Inbox::new(self_check::asks_dir(&ws.root));
        let answerer = std::thread::spawn(move || {
            for _ in 0..200 {
                if let Some(r) = asks.pending().first() {
                    asks.answer(&r.id, &Answer { text: Some("build: passed — Build succeeded.".into()), ..Default::default() }).unwrap();
                    return true;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            false
        });
        // The project comes from the tool server's inbox (<project>/inbox) when not given.
        e.session().ext.insert(Arc::new(InboxDir(ws.inbox_dir())));
        let v = e.call("rebuild_check", json!({"wait_minutes": 1}), ok()).unwrap().to_json();
        assert!(answerer.join().unwrap(), "the request reached checks/asks/");
        assert_eq!(v["result"]["report"], "build: passed — Build succeeded.");
        // A build running in this process can't answer a check asked from inside it.
        let _here = self_check::BuildingHere::new(&ws.root);
        let err = e.call("rebuild_check", json!({"project": ws.root}), ok()).err().unwrap();
        assert_eq!(err.code, ErrorCode::FeatureMissing);
    }
}
