//! The builder tests its own work while it builds: `rebuild_check` (in [`super::tools`]) asks the
//! run through `checks/asks/` (the inbox's folder protocol, in a folder only the run reads), and
//! the run answers during the builder's turn with the same checks it runs after the turn, as
//! round 0 (`checks/round-0/`), not counted as a round.
//!
//! The check runs on a thread of its own, so the run keeps relaying the builder's events while
//! it works, says every few minutes that it's still going, and stops it before `rebuild_check`'s
//! own wait ends so the builder gets an answer either way. At most [`SELF_CHECK_LIMIT`] checks
//! run in one turn; after that the builder is told to finish the turn and let the run check.

use super::*;
use serde::{Deserialize, Serialize};
use std::thread::JoinHandle;

/// The round a builder's own check is run as; real rounds start at 1.
pub const SELF_CHECK_ROUND: u32 = 0;

/// Most checks the builder may ask for in one turn. Beyond it, each ask is answered with a note
/// to finish the turn: the run checks after it anyway, and a builder that re-checks without end
/// never finishes (brief step 4 says so too).
pub const SELF_CHECK_LIMIT: u32 = 5;

/// How often a running check says it's still going.
const STILL_CHECKING_EVERY: Duration = Duration::from_secs(150);

/// How long before `rebuild_check` gives up waiting the run stops the check, so its answer gets
/// there first.
const ANSWER_GRACE: Duration = Duration::from_secs(30);

/// What `rebuild_check` puts in its request's `detail`: how long it waits for the answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckAsk {
    pub wait_secs: u64,
}

impl CheckAsk {
    /// What an ask without a wait (an older tool server) is taken to wait: the tool's default.
    pub const DEFAULT_WAIT: Duration = Duration::from_secs(20 * 60);

    /// The request detail for a tool that waits `wait`.
    pub fn detail(wait: Duration) -> String {
        serde_json::to_string(&CheckAsk { wait_secs: wait.as_secs() }).unwrap_or_default()
    }

    /// The ask a request's detail carries; the default wait when it carries none.
    pub fn parse(detail: &str) -> CheckAsk {
        serde_json::from_str(detail).unwrap_or(CheckAsk { wait_secs: Self::DEFAULT_WAIT.as_secs() })
    }

    /// How long the run lets the check go: the tool's wait less a margin for the answer to
    /// arrive, and never under half the wait.
    pub fn check_deadline(self) -> Duration {
        let wait = Duration::from_secs(self.wait_secs);
        wait.saturating_sub(ANSWER_GRACE).max(wait / 2)
    }
}

/// The builder's own checks in a run: how they're judged, the one running now and how many this
/// turn has had.
pub(super) struct SelfCheck {
    pub(super) kind: Arc<dyn kinds::TargetKind>,
    pub(super) good_enough: f64,
    running: Option<RunningSelfCheck>,
    pub(super) asked_this_turn: u32,
    /// Asks answered this turn, so one listed again (its files not yet taken by the builder)
    /// isn't answered twice.
    answered: std::collections::HashSet<String>,
}

impl SelfCheck {
    pub(super) fn new(kind: Arc<dyn kinds::TargetKind>, good_enough: f64) -> SelfCheck {
        SelfCheck { kind, good_enough, running: None, asked_this_turn: 0, answered: std::collections::HashSet::new() }
    }
}

/// A check running on its own thread for one of the builder's asks.
struct RunningSelfCheck {
    id: String,
    started: Instant,
    deadline: Duration,
    stop: CancelToken,
    handle: JoinHandle<Result<Vec<CheckResult>, ToolError>>,
    last_said: Instant,
    what: String,
    /// The ask was answered already (the deadline passed), so the result is dropped when it ends.
    answered: bool,
}

/// What the builder is told when its check ran past `rebuild_check`'s wait.
pub fn timed_out_report(elapsed: Duration) -> String {
    format!("The check ran out of time after {} minutes; finish the turn and Craft Anything will check it after.", minutes(elapsed))
}

/// What the builder is told when it asks for a check past [`SELF_CHECK_LIMIT`] in one turn.
pub fn over_limit_report() -> String {
    format!(
        "You've already checked {SELF_CHECK_LIMIT} times this turn, the most for one turn, so this check didn't run. Finish the turn: Craft Anything checks the build after it and tells you what differs."
    )
}

/// The feed line while a check is still going ("Still checking: 7 min, building with cargo…").
pub fn still_checking_line(elapsed: Duration, what: &str) -> String {
    format!("Still checking: {} min, {what}…", minutes(elapsed))
}

/// Whole minutes, at least one.
fn minutes(d: Duration) -> u64 {
    (d.as_secs() / 60).max(1)
}

/// What a check of this project spends its time on, for the still-going line.
pub fn checking_what(ws: &Workspace) -> String {
    let app = ws.app_dir();
    match game::Builder::from_stack(&ws.manifest.stack) {
        Some(b) if b != game::Builder::Rust => format!("building with {}", b.name()),
        _ if app.join("Cargo.toml").is_file() => "building with cargo".into(),
        _ if app.join("package.json").is_file() => "building with npm".into(),
        _ if ws.manifest.target == TargetRef::None => "testing the build".into(),
        _ => "comparing it with the original".into(),
    }
}

/// Where `rebuild_check` asks a project's run for a check.
pub fn asks_dir(project: &Path) -> PathBuf {
    project.join("checks").join("asks")
}

/// Projects whose build runs in this process. A builder in this process (an API backend) calls
/// tools with the engine locked, so its `rebuild_check` could never be answered by this run.
static BUILDING_HERE: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Marks a project as building in this process until dropped.
pub(super) struct BuildingHere(PathBuf);

impl BuildingHere {
    pub(super) fn new(root: &Path) -> BuildingHere {
        BUILDING_HERE.lock().unwrap_or_else(PoisonError::into_inner).push(root.to_path_buf());
        BuildingHere(root.to_path_buf())
    }
}

impl Drop for BuildingHere {
    fn drop(&mut self) {
        let mut v = BUILDING_HERE.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(i) = v.iter().position(|p| *p == self.0) {
            v.remove(i);
        }
    }
}

/// The project builds in this process.
pub fn building_here(root: &Path) -> bool {
    BUILDING_HERE.lock().unwrap_or_else(PoisonError::into_inner).iter().any(|p| p == root)
}

/// The questions the builder recorded (`core_unknown_add`, kept in the project's investigation
/// folder) that aren't resolved yet, most severe first. Read from disk: the builder's tool server
/// writes them while this run works.
pub fn open_questions(ws: &Workspace) -> Vec<craft_anything_evidence::Unknown> {
    let mut open: Vec<_> = Project::open(&ws.investigation_dir())
        .and_then(|p| p.load_unknowns())
        .unwrap_or_default()
        .into_iter()
        .filter(|u| u.status != craft_anything_evidence::UnknownStatus::Resolved)
        .collect();
    open.sort_by(|a, b| b.severity.cmp(&a.severity).then_with(|| a.question.cmp(&b.question)));
    open
}

/// One plain line on how a round of checks went: how alike when compared, else how many checks
/// pass, and how many of their claims hold ("about 93% like the original, 5 of 5 claims hold";
/// "all 4 checks pass, 5 of 5 claims hold").
pub fn summary(checks: &[CheckResult], good_enough: f64) -> String {
    let claims: Vec<_> = checks.iter().flat_map(|c| &c.claims).collect();
    let held = claims.iter().filter(|c| c.passed).count();
    let claims_part = if claims.is_empty() { String::new() } else { format!(", {held} of {} claims hold", claims.len()) };
    let good = checks.iter().filter(|c| c.good(good_enough)).count();
    match checks.iter().find_map(|c| c.similarity) {
        Some(s) => format!("about {s:.0}% like the original{claims_part}"),
        None if good == checks.len() => format!("all {} checks pass{claims_part}", checks.len()),
        None => format!("{good} of {} checks pass{claims_part}", checks.len()),
    }
}

/// What the builder is told about its own check: each result with its likeness and the bar,
/// what's still unknown (the checks' and its own open questions), where its pictures are, and
/// whether it would pass.
pub fn report(checks: &[CheckResult], good_enough: f64, open: &[craft_anything_evidence::Unknown]) -> String {
    if checks.is_empty() {
        return "There was nothing to check yet.".into();
    }
    let mut lines = Vec::new();
    for c in checks {
        let mut line = format!("{}: {}", c.check, if c.passed { "passed" } else { "failed" });
        if let Some(s) = c.similarity {
            line.push_str(&format!(", {s:.0}% alike (needs {:.0}%)", c.good_enough.unwrap_or(good_enough)));
        }
        line.push_str(&format!(" — {}", c.note));
        lines.push(line);
    }
    let failed: Vec<String> = checks.iter().flat_map(|c| c.failed_claims()).map(|c| format!("{} ({})", c.title, c.detail)).collect();
    if !failed.is_empty() {
        lines.push(format!("These must hold before it can pass: {}.", failed.join("; ")));
    }
    let unknowns: Vec<&str> = checks.iter().flat_map(|c| &c.unknowns).map(String::as_str).collect();
    if !unknowns.is_empty() {
        lines.push(format!("Not compared, so it can't pass yet: {}. Make those parts exist and render, then check again.", unknowns.join(", ")));
    }
    if !open.is_empty() {
        let shown: Vec<String> = open.iter().take(8).map(|u| format!("{} ({})", u.question, u.id)).collect();
        lines.push(format!(
            "Your open questions: {}. Settle what one more look can and close it with core_unknown_edit (status resolved, the resolution and the evidence ids); say what's left in your summary.",
            shown.join("; ")
        ));
    }
    let pictures: Vec<String> = checks.iter().flat_map(|c| &c.artifacts).map(|p| p.display().to_string().replace('\\', "/")).collect();
    if !pictures.is_empty() {
        lines.push(format!("Pictures of your build (compare each with the original's in capture/ or reference/): {}", pictures.join(", ")));
    }
    lines.push(if checks.iter().all(|c| c.good(good_enough)) {
        "This would pass. Finish what you're doing, then reply.".to_string()
    } else {
        "Not good enough yet: fix what it lists, build again, and check again with rebuild_check.".to_string()
    });
    lines.join("\n")
}

impl Worker {
    /// Answer the builder's own check requests during its turn (once the build loop has started):
    /// start the next one on its own thread, say when a running one is still going, stop one
    /// whose ask has run out of time, and hand a finished one's report back. Never blocks the
    /// turn's poll loop.
    pub(super) fn answer_self_checks(&mut self, ws: &mut Workspace) -> Result<(), ToolError> {
        let Some(mut sc) = self.self_check.take() else { return Ok(()) };
        let r = self.drive_self_checks(ws, &mut sc);
        self.self_check = Some(sc);
        r
    }

    fn drive_self_checks(&self, ws: &mut Workspace, sc: &mut SelfCheck) -> Result<(), ToolError> {
        let asks = Inbox::new(asks_dir(&ws.root));
        if let Some(run) = sc.running.as_mut() {
            if self.stop.is_cancelled() {
                run.stop.cancel();
            }
            if run.handle.is_finished() {
                let Some(run) = sc.running.take() else { return Ok(()) };
                let result = run.handle.join().unwrap_or_else(|_| Err(ToolError::internal("The check hit a bug and stopped safely.")));
                if run.answered {
                    return Ok(());
                }
                let (text, line) = match result {
                    Ok(checks) => {
                        let line = format!("The builder checked its work: {}.", summary(&checks, sc.good_enough));
                        (report(&checks, sc.good_enough, &open_questions(ws)), line)
                    }
                    Err(e) if e.code == ErrorCode::Cancelled => return Err(e),
                    Err(e) => (format!("The check couldn't run: {}", e.message), "The builder's check couldn't run.".to_string()),
                };
                asks.answer(&run.id, &Answer { text: Some(text), ..Default::default() })?;
                return self.step(ws, Step::Build, Status::Building, &line);
            }
            let elapsed = run.started.elapsed();
            if !run.answered && elapsed >= run.deadline {
                // The tool stops waiting soon: answer now, and stop the check (its result would
                // reach nobody). The run checks the build after the turn as usual.
                run.stop.cancel();
                run.answered = true;
                asks.answer(&run.id, &Answer { text: Some(timed_out_report(elapsed)), ..Default::default() })?;
                let line = format!("The builder's check ran out of time after {} min; the build is checked after the turn.", minutes(elapsed));
                return self.step(ws, Step::Build, Status::Building, &line);
            }
            if run.last_said.elapsed() >= STILL_CHECKING_EVERY {
                run.last_said = Instant::now();
                let line = still_checking_line(elapsed, &run.what);
                return self.step(ws, Step::Build, Status::Building, &line);
            }
            return Ok(());
        }
        for r in asks.pending() {
            if !sc.answered.insert(r.id.clone()) {
                continue;
            }
            if sc.asked_this_turn >= SELF_CHECK_LIMIT {
                asks.answer(&r.id, &Answer { text: Some(over_limit_report()), ..Default::default() })?;
                let line = format!("The builder asked for another check after {SELF_CHECK_LIMIT} this turn; the build is checked after the turn instead.");
                self.step(ws, Step::Build, Status::Building, &line)?;
                continue;
            }
            sc.asked_this_turn = sc.asked_this_turn.saturating_add(1);
            let checking = if ws.manifest.target == TargetRef::None {
                "The builder is checking its work…"
            } else {
                "The builder is checking its work against the original…"
            };
            self.step(ws, Step::Build, Status::Building, checking)?;
            let stop = CancelToken::default();
            let twin = self.twin(stop.clone());
            let (ws2, kind) = (ws.clone(), sc.kind.clone());
            let spawned = std::thread::Builder::new().name("self-check".into()).spawn(move || twin.run_checks(&ws2, kind.as_ref(), SELF_CHECK_ROUND));
            match spawned {
                Ok(handle) => {
                    let now = Instant::now();
                    sc.running = Some(RunningSelfCheck {
                        id: r.id.clone(),
                        started: now,
                        deadline: CheckAsk::parse(&r.detail).check_deadline(),
                        stop,
                        handle,
                        last_said: now,
                        what: checking_what(ws),
                        answered: false,
                    });
                    return Ok(());
                }
                Err(e) => {
                    asks.answer(&r.id, &Answer { text: Some(format!("The check couldn't run: {e}")), ..Default::default() })?;
                    self.step(ws, Step::Build, Status::Building, "The builder's check couldn't run.")?;
                }
            }
        }
        Ok(())
    }

    /// End the builder's own check if one is still running: the turn is over (or the run is),
    /// and the run's own checks must not overlap it. Its ask, if unanswered, gets the timed-out
    /// answer so nothing waits on it; an answer that can't be written is this run's error (the
    /// project's inbox is broken), never the builder's.
    pub(super) fn finish_self_checks(&mut self, ws: &Workspace) -> Result<(), ToolError> {
        let Some(run) = self.self_check.as_mut().and_then(|sc| sc.running.take()) else { return Ok(()) };
        run.stop.cancel();
        let answered = if run.answered {
            Ok(())
        } else {
            Inbox::new(asks_dir(&ws.root)).answer(&run.id, &Answer { text: Some(timed_out_report(run.started.elapsed())), ..Default::default() })
        };
        let _ = run.handle.join();
        answered
    }

    /// A worker for the check thread: the same engine, feed and preview as this run, with its own
    /// stop (cancelled when the ask runs out of time or the run stops).
    fn twin(&self, stop: CancelToken) -> Worker {
        Worker {
            wake: self.wake.clone(),
            engine: self.engine.clone(),
            tx: self.tx.clone(),
            stop,
            preview: self.preview.clone(),
            seen: HashSet::new(),
            inbox: self.inbox.clone(),
            editor: self.editor.clone(),
            notes: self.notes.clone(),
            feed: self.feed.clone(),
            assets: self.assets,
            decompiled: self.decompiled,
            usage: None,
            self_check: None,
            reset_progress_after_notes: false,
            turn_close_grace: TURN_CLOSE_GRACE,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_report_says_what_differs_and_whether_it_would_pass() {
        let web = CheckResult {
            similarity: Some(82.4),
            artifacts: vec![PathBuf::from("checks/round-0/screenshot-390.png")],
            ..CheckResult::new(CheckResult::WEB_VISUAL, SELF_CHECK_ROUND, true, "at 390 px: the page scrolls sideways (512 px wide)")
        };
        let r = report(std::slice::from_ref(&web), 90.0, &[]);
        assert!(r.contains("82% alike (needs 90%) — at 390 px: the page scrolls sideways") && r.contains("checks/round-0/screenshot-390.png"), "{r}");
        assert!(r.ends_with("check again with rebuild_check."), "{r}");
        let good = CheckResult { similarity: Some(93.0), ..web };
        assert!(report(&[good], 90.0, &[]).ends_with("This would pass. Finish what you're doing, then reply."));
        let build = CheckResult::new(CheckResult::BUILD, SELF_CHECK_ROUND, false, "Build failed: error[E0425]");
        let r = report(&[build], 90.0, &[]);
        assert!(r.starts_with("build: failed — Build failed") && r.contains("Not good enough yet"), "{r}");
        assert_eq!(report(&[], 90.0, &[]), "There was nothing to check yet.");
        let claim = |passed| craft_anything_engine::workspace::Claim::new("In play, the player moves", passed, "it doesn't");
        let game = [
            CheckResult::new(CheckResult::BUILD, 1, true, "Build succeeded."),
            CheckResult { claims: vec![claim(true), claim(true)], ..CheckResult::new("game-self-test", 1, true, "") },
            CheckResult::new("mechanics", 1, true, "app/mechanics.json has 15 values"),
        ];
        assert_eq!(summary(&game, 90.0), "all 3 checks pass, 2 of 2 claims hold");
        let failing = [game[0].clone(), CheckResult { claims: vec![claim(true), claim(false)], ..CheckResult::new("game-self-test", 1, false, "") }];
        assert_eq!(summary(&failing, 90.0), "1 of 2 checks pass, 1 of 2 claims hold");
        let site = [CheckResult { similarity: Some(93.4), ..CheckResult::new(CheckResult::WEB_VISUAL, 1, true, "") }];
        assert_eq!(summary(&site, 90.0), "about 93% like the original");
        let alike_but_broken = CheckResult {
            similarity: Some(96.0),
            claims: vec![craft_anything_engine::workspace::Claim::new("The start page loads every file it asks for", false, "/logo.png (404)")],
            ..CheckResult::new(CheckResult::WEB_VISUAL, 0, true, "")
        };
        let r = report(&[alike_but_broken], 90.0, &[]);
        assert!(
            r.contains("These must hold before it can pass: The start page loads every file it asks for (/logo.png (404))")
                && r.contains("Not good enough yet"),
            "{r}"
        );
        let blind = CheckResult {
            similarity: Some(95.0),
            unknowns: vec!["/about at 768 px couldn't be compared".into()],
            ..CheckResult::new(CheckResult::WEB_VISUAL, 0, true, "")
        };
        let r = report(&[blind], 90.0, &[]);
        assert!(r.contains("Not compared, so it can't pass yet: /about at 768 px") && r.contains("Not good enough yet"), "an unknown never passes: {r}");
    }

    #[test]
    fn the_builders_open_questions_are_read_from_the_project_and_reported() {
        use craft_anything_evidence::{Severity, Unknown, UnknownStatus};
        let d = tempfile::tempdir().unwrap();
        let ws = Workspace::create(d.path(), "Arena", "Recreate this game", TargetRef::None).unwrap();
        assert!(open_questions(&ws).is_empty(), "no investigation folder yet");
        let project = Project::create(&ws.investigation_dir(), "Arena", Purpose::default()).unwrap();
        project.store_unknown(&Unknown::new("Jump height?", "game", Severity::Low)).unwrap();
        project.store_unknown(&Unknown::new("Which font is the menu set in?", "ui", Severity::High)).unwrap();
        let mut done = Unknown::new("Map size?", "game", Severity::Critical);
        done.status = UnknownStatus::Resolved;
        project.store_unknown(&done).unwrap();
        let open = open_questions(&ws);
        let questions: Vec<&str> = open.iter().map(|u| u.question.as_str()).collect();
        assert_eq!(questions, ["Which font is the menu set in?", "Jump height?"], "unresolved only, most severe first");
        let r = report(&[CheckResult::new(CheckResult::BUILD, 0, true, "Build succeeded.")], 90.0, &open);
        assert!(r.contains("Your open questions: Which font is the menu set in? (unk_") && r.contains("close it with core_unknown_edit"), "{r}");
    }

    #[test]
    fn an_ask_carries_its_wait_and_the_run_stops_the_check_before_it_ends() {
        let ask = CheckAsk::parse(&CheckAsk::detail(Duration::from_secs(20 * 60)));
        assert_eq!(ask.wait_secs, 1200);
        assert_eq!(ask.check_deadline(), Duration::from_secs(1170), "half a minute before the tool gives up");
        assert_eq!(CheckAsk::parse("").wait_secs, 1200, "an older tool server's ask waits the default");
        assert_eq!(CheckAsk::parse("not json").check_deadline(), Duration::from_secs(1170));
        assert_eq!(CheckAsk { wait_secs: 60 }.check_deadline(), Duration::from_secs(30));
        assert_eq!(CheckAsk { wait_secs: 1 }.check_deadline(), Duration::from_millis(500), "never under half the wait");
        assert_eq!(still_checking_line(Duration::from_secs(7 * 60 + 20), "building with cargo"), "Still checking: 7 min, building with cargo…");
        assert_eq!(still_checking_line(Duration::from_secs(5), "testing the build"), "Still checking: 1 min, testing the build…");
        assert!(timed_out_report(Duration::from_secs(19 * 60 + 40)).starts_with("The check ran out of time after 19 minutes; finish the turn"));
        assert!(over_limit_report().contains("5 times this turn") && over_limit_report().contains("Finish the turn"));
        let d = tempfile::tempdir().unwrap();
        let ws = Workspace::create(d.path(), "Arena", "Recreate this game", TargetRef::None).unwrap();
        assert_eq!(checking_what(&ws), "testing the build");
        std::fs::create_dir_all(ws.app_dir()).unwrap();
        std::fs::write(ws.app_dir().join("Cargo.toml"), "[package]").unwrap();
        assert_eq!(checking_what(&ws), "building with cargo");
        let mut godot = Workspace::create(d.path(), "Tower", "Recreate this game", TargetRef::None).unwrap();
        godot.manifest.stack = "Godot".into();
        assert_eq!(checking_what(&godot), "building with Godot");
    }

    #[test]
    fn a_project_is_marked_as_building_here_only_while_its_run_lasts() {
        let root = PathBuf::from("/projects/marked-here");
        assert!(!building_here(&root));
        let mark = BuildingHere::new(&root);
        assert!(building_here(&root));
        drop(mark);
        assert!(!building_here(&root));
    }
}
