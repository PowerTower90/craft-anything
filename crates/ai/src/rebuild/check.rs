//! Building a project with its own tools (cargo, npm, or the game engine's), for [`BuildCheck`].
//! A plain static site has no build, and passes as it is. A person's own project builds and tests
//! itself the way its toolchain says.

use super::*;

/// Where a long build step says it's still going (a feed line); the silent one for callers
/// without a feed.
pub(super) type Say<'a> = &'a dyn Fn(String);

/// Says nothing: for tests without a feed.
#[cfg(test)]
pub(super) fn quietly(_: String) {}

/// How often a running build step says it's still going.
const STILL_BUILDING_EVERY: Duration = Duration::from_secs(150);

/// Minutes a toolchain's build (cargo, npm, a person's own) may print nothing before it's
/// stopped: a cold build prints something within that, and a hung one never will.
const TOOLCHAIN_IDLE_MINUTES: u64 = 15;

/// Minutes a game engine's build may print nothing: shader compiles and cooking go quiet for
/// long stretches.
const ENGINE_IDLE_MINUTES: u64 = 60;

/// Run a step of a project's build in `app`: whether it succeeded, and what it printed. While it
/// runs, every few minutes `say` gets a line that it's still going.
fn run_in(app: &Path, prog: &Path, args: &[String], minutes: u64, idle_minutes: u64, stop: &CancelToken, say: Say) -> Result<(bool, String), String> {
    let s = stop.clone();
    let name = prog.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| prog.display().to_string());
    let mut running = craft_anything_platform::Process::new(prog)
        .args(args)
        .cwd(app)
        .timeout(Duration::from_secs(minutes * 60))
        .idle_timeout(Duration::from_secs(idle_minutes * 60))
        .stdout_cap(8 << 20)
        .cancel_when(move || s.is_cancelled())
        .spawn()
        .map_err(|e| e.message)?;
    let started = Instant::now();
    let mut said = started;
    while running.wait_timeout(Duration::from_millis(500)).is_none() {
        if said.elapsed() >= STILL_BUILDING_EVERY {
            said = Instant::now();
            say(still_building_line(started.elapsed(), &name));
        }
    }
    running.wait().map(|o| (o.success, format!("{}{}", o.stdout_text(), o.stderr_text()))).map_err(|e| e.message)
}

/// The feed line while a build step is still going ("Still building with cargo: 7 min so far…").
pub(super) fn still_building_line(elapsed: Duration, program: &str) -> String {
    format!("Still building with {program}: {} min so far…", (elapsed.as_secs() / 60).max(1))
}

/// Run one of a toolchain's commands in `app`.
fn run_tool(app: &Path, tool: &stacks::Toolchain, command: &[String], minutes: u64, stop: &CancelToken, say: Say) -> Result<(bool, String), String> {
    let Some((program, args)) = tool.command(app, command) else { return Err(format!("{} has no such command.", tool.name)) };
    run_in(app, &program, &args, minutes, TOOLCHAIN_IDLE_MINUTES, stop, say)
}

/// The package.json script an `npm run <script>` command runs, if the command is one.
fn npm_run_script(command: &[String]) -> Option<&str> {
    match command {
        [npm, run, script, ..] if run == "run" && Path::new(npm).file_stem().is_some_and(|n| n == "npm") => Some(script.as_str()),
        _ => None,
    }
}

/// Programs in a package.json script that serve or watch and never exit on their own, unless a
/// `build` word makes them a one-off build (`vite build`).
const SERVER_PROGRAMS: &[&str] = &["vite", "serve", "nodemon", "http-server", "live-server", "webpack-dev-server", "browser-sync", "sirv", "wrangler"];

/// `program subcommand` pairs that start a server.
const SERVER_SUBCOMMANDS: &[(&str, &str)] = &[
    ("next", "dev"),
    ("next", "start"),
    ("nuxt", "dev"),
    ("nuxt", "preview"),
    ("astro", "dev"),
    ("astro", "preview"),
    ("remix", "dev"),
    ("vite", "preview"),
    ("vite", "dev"),
    ("vite", "serve"),
    ("webpack", "serve"),
    ("webpack", "watch"),
    ("parcel", "serve"),
    ("parcel", "watch"),
    ("ng", "serve"),
    ("tsc", "--watch"),
    ("npm", "start"),
    ("pnpm", "dev"),
    ("pnpm", "start"),
    ("yarn", "dev"),
    ("yarn", "start"),
    ("bun", "dev"),
];

/// Words before the program in a script step that aren't it: runners and environment settings.
fn is_script_prefix(word: &str) -> bool {
    matches!(word, "npx" | "cross-env" | "dotenv" | "env" | "bunx" | "pnpx" | "exec" | "--")
        || (word.contains('=') && word.split('=').next().is_some_and(|k| !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')))
}

/// Whether a package.json script starts a dev server or watcher: run as a build, it would never
/// exit, and the build check would wait on it until its time limit (#phase1-hangs).
pub(super) fn script_never_exits(script: &str) -> bool {
    script.replace("&&", ";").replace("||", ";").split([';', '|']).any(step_never_exits)
}

fn step_never_exits(step: &str) -> bool {
    let words: Vec<&str> = step.split_whitespace().skip_while(|w| is_script_prefix(w)).collect();
    let Some(program) = words.first().map(|p| p.rsplit(['/', '\\']).next().unwrap_or(p)) else { return false };
    let second = words.get(1).copied().unwrap_or_default();
    if words.iter().any(|w| *w == "--watch" || (*w == "-w" && program == "tsc")) {
        return true;
    }
    // `npm run dev` (and its siblings) as a step of the build script.
    if matches!(program, "npm" | "pnpm" | "yarn" | "bun")
        && second == "run"
        && matches!(words.get(2).copied().unwrap_or_default(), "dev" | "start" | "serve" | "watch" | "preview")
    {
        return true;
    }
    if SERVER_SUBCOMMANDS.contains(&(program, second)) {
        return true;
    }
    SERVER_PROGRAMS.contains(&program) && !words.contains(&"build")
}

/// The note when `app`'s package.json script `name` would never exit; `None` when it's a build
/// (or there's no such script).
fn dev_server_script(app: &Path, name: &str) -> Option<String> {
    let script = craft_anything_engine::outputs::npm_script(app, name).filter(|s| script_never_exits(s))?;
    Some(format!(
        "The build didn't run: package.json's `{name}` script is `{script}`, which starts a server or watcher and would never exit. Make it a build that finishes (such as `vite build` or `next build`), then build again."
    ))
}

/// What a Unity without a licence prints, and what to tell the person.
const UNITY_NO_LICENCE: &str = "No valid Unity Editor license";
const UNITY_LICENCE_FIX: &str = "Unity needs its free licence first: open Unity Hub, sign in and accept the Personal licence, then try again.";

/// A person's own project: its engine's build, else its toolchain's, else a static site. One we
/// know no way to build isn't a failure, and the note says nothing ran.
#[cfg(test)]
pub(super) fn own_build_check(app: &Path, place: &str, stack: &str, editor: Option<&Path>, stop: &CancelToken) -> (bool, String) {
    own_build_check_saying(app, place, stack, editor, stop, &quietly)
}

/// [`own_build_check`], saying through `say` every few minutes that it's still going.
pub(super) fn own_build_check_saying(app: &Path, place: &str, stack: &str, editor: Option<&Path>, stop: &CancelToken, say: Say) -> (bool, String) {
    let engine = engine_for(stack, app);
    if let (Some(game::Builder::Unity), Some(ed)) = (engine, editor) {
        return own_unity_check(app, ed, stop, say);
    }
    match stacks::toolchain_in(app).filter(|_| engine.is_none()) {
        Some(tool) => own_build(app, tool, stop, say),
        None if engine.is_some() || static_site(app) => build_check_saying(app, place, stack, editor, stop, say),
        None => (true, "Nothing was built: Craft Anything doesn't know how to build this kind of project.".into()),
    }
}

/// How a Unity project we didn't make is opened: in batch mode, which compiles its scripts. There's
/// no `CraftBuild.Build` to call, as there is in one we made ([`build_check`]).
fn own_unity_args(app: &Path) -> Vec<String> {
    ["-batchmode", "-quit", "-nographics", "-projectPath", &app.display().to_string(), "-logFile", "-"].map(String::from).to_vec()
}

fn own_unity_check(app: &Path, editor: &Path, stop: &CancelToken, say: Say) -> (bool, String) {
    match run_in(app, editor, &own_unity_args(app), 4 * 60, ENGINE_IDLE_MINUTES, stop, say) {
        Ok((_, out)) if out.contains(UNITY_NO_LICENCE) => (false, UNITY_LICENCE_FIX.into()),
        Ok((true, _)) => (true, "The Unity project opened and its scripts compiled.".into()),
        Ok((false, out)) => (false, format!("Build failed: {}", error_lines(&out))),
        Err(e) => (false, format!("Couldn't run the build: {e}")),
    }
}

fn own_build(app: &Path, tool: &stacks::Toolchain, stop: &CancelToken, say: Say) -> (bool, String) {
    // A Node project whose build script serves or watches would be waited on for two hours.
    if let Some(note) = npm_run_script(&tool.build).and_then(|name| dev_server_script(app, name)) {
        return (false, note);
    }
    match run_tool(app, tool, &tool.build, 2 * 60, stop, say) {
        Ok((true, _)) => (true, format!("`{}` passed.", tool.build.join(" "))),
        Ok((false, out)) => (false, format!("Build failed: {}", error_lines(&out))),
        Err(e) => (false, format!("Couldn't run the build: {e}")),
    }
}

/// A person's own project: run its tests. Output that says none ran isn't a failure, and neither
/// is a project we know no way to test, and the note says so.
#[cfg(test)]
pub(super) fn own_tests_check(app: &Path, tool: Option<&stacks::Toolchain>, stop: &CancelToken) -> (bool, String) {
    own_tests_check_saying(app, tool, stop, &quietly)
}

/// [`own_tests_check`], saying through `say` every few minutes that it's still going.
pub(super) fn own_tests_check_saying(app: &Path, tool: Option<&stacks::Toolchain>, stop: &CancelToken, say: Say) -> (bool, String) {
    let Some(tool) = tool else { return (true, "No tests ran: Craft Anything doesn't know how to test this kind of project.".into()) };
    let says_none = |text: &str| tool.no_tests.iter().find(|n| text.contains(n.as_str())).cloned();
    if let Some(name) = &tool.test_script {
        // shortcut: `workspaces=true` in .npmrc can give a root with no script tests; they aren't
        // run, and the note says so. Read .npmrc if that turns up.
        let script = craft_anything_engine::outputs::npm_script(app, name).unwrap_or_default();
        if script.is_empty() || tool.no_tests.iter().any(|n| n == script.trim()) {
            return (true, format!("No tests ran: package.json has no `{name}` script to run."));
        }
    }
    match run_tool(app, tool, &tool.test, 30, stop, say) {
        Ok((true, _)) => (true, format!("`{}` passed.", tool.test.join(" "))),
        Ok((false, out)) => match says_none(&out).filter(|_| tool.test_script.is_none()) {
            Some(why) => (true, format!("No tests ran: {}.", why.trim_end_matches('.'))),
            None => (false, format!("The project's tests failed:\n{}", last_lines(&out, 20))),
        },
        Err(e) => (false, format!("Couldn't run the tests: {e}")),
    }
}

/// The last `n` lines with something on them: where a test run sums up what failed.
fn last_lines(out: &str, n: usize) -> String {
    let mut lines: Vec<&str> = out.lines().rev().filter(|l| !l.trim().is_empty()).take(n).collect();
    lines.reverse();
    lines.join("\n")
}

/// For non-website builds: run the project's build (its engine's own, for Unity, Unreal and
/// Godot) and report whether it succeeded, with a note (the first error lines when it didn't).
/// `place` says where `app` is, for messages (`Workspace::app_place`).
#[cfg(test)]
pub(super) fn build_check(app: &Path, place: &str, stack: &str, editor: Option<&Path>, stop: &CancelToken) -> (bool, String) {
    build_check_saying(app, place, stack, editor, stop, &quietly)
}

/// [`build_check`], saying through `say` every few minutes that it's still going.
pub(super) fn build_check_saying(app: &Path, place: &str, stack: &str, editor: Option<&Path>, stop: &CancelToken, say: Say) -> (bool, String) {
    let run = |prog: &Path, args: &[String], minutes: u64, idle: u64| {
        run_in(app, prog, args, minutes, idle, stop, say).map_err(|e| format!("Couldn't run the build: {e}"))
    };
    let app_s = app.display().to_string();
    let built = || packaged_program(&app.join("Build")).is_some();
    let result: Result<(bool, String), String> = match (engine_for(stack, app), editor) {
        (Some(game::Builder::Unity), Some(ed)) => {
            if !app.join("Assets").is_dir() {
                return (false, missing(app, place, format!("No Unity project found in {place} yet.")));
            }
            let args = ["-batchmode", "-quit", "-nographics", "-projectPath", &app_s, "-executeMethod", "CraftBuild.Build", "-logFile", "-"].map(String::from);
            run(ed, &args, 4 * 60, ENGINE_IDLE_MINUTES)
                .map(|(ok, out)| if out.contains(UNITY_NO_LICENCE) { (false, UNITY_LICENCE_FIX.into()) } else { (ok && built(), out) })
        }
        (Some(game::Builder::Unreal), Some(ed)) => {
            let Some(uproject) = uproject(app) else {
                return (false, missing(app, place, format!("No Unreal project (.uproject) found in {place} yet.")));
            };
            // The editor is <engine>/Engine/Binaries/<platform>/UnrealEditor; its packager is beside it.
            let Some(root) = ed.ancestors().nth(4) else { return (false, "Couldn't find Unreal's build tools.".into()) };
            let uat = root.join("Engine/Build/BatchFiles").join(if cfg!(windows) { "RunUAT.bat" } else { "RunUAT.sh" });
            let platform = if cfg!(windows) {
                "Win64"
            } else if cfg!(target_os = "macos") {
                "Mac"
            } else {
                "Linux"
            };
            let args = vec![
                "BuildCookRun".to_string(),
                format!("-project={}", uproject.display()),
                format!("-platform={platform}"),
                "-clientconfig=Development".into(),
                "-build".into(),
                "-cook".into(),
                "-stage".into(),
                "-pak".into(),
                "-archive".into(),
                format!("-archivedirectory={}", app.join("Build").display()),
                "-unattended".into(),
                "-utf8output".into(),
                "-nop4".into(),
            ];
            // A first Unreal build compiles and cooks for a long while. Run RunUAT.bat directly:
            // std quotes .bat launches itself, whereas `cmd /C "<path with spaces>" "<arg>"` strips the quotes.
            run(&uat, &args, 8 * 60, ENGINE_IDLE_MINUTES).map(|(ok, out)| (ok && built(), out))
        }
        (Some(game::Builder::Godot), Some(ed)) => {
            if !app.join("project.godot").is_file() {
                return (false, missing(app, place, format!("No Godot project (project.godot) found in {place} yet.")));
            }
            let import = ["--headless", "--path", &app_s, "--import"].map(String::from);
            let play = ["--headless", "--path", &app_s, "--quit-after", "120"].map(String::from);
            // Godot exits cleanly even when scripts fail, so its error lines decide.
            run(ed, &import, 60, ENGINE_IDLE_MINUTES).and_then(|(_, a)| run(ed, &play, 10, ENGINE_IDLE_MINUTES).map(|(ok, b)| (ok, a + &b))).map(|(ok, out)| {
                let bad = out.lines().any(|l| l.contains("SCRIPT ERROR") || l.contains("Parse Error") || l.trim_start().starts_with("ERROR:"));
                (ok && !bad, out)
            })
        }
        _ if app.join("Cargo.toml").is_file() => run(Path::new("cargo"), &["build".into(), "--quiet".into()], 2 * 60, TOOLCHAIN_IDLE_MINUTES),
        // A `build` script that serves or watches is never started: the check would wait on it for two hours.
        _ if app.join("package.json").is_file() => match dev_server_script(app, "build") {
            Some(note) => return (false, note),
            None => run(Path::new(if cfg!(windows) { "npm.cmd" } else { "npm" }), &["run".into(), "build".into()], 2 * 60, TOOLCHAIN_IDLE_MINUTES),
        },
        (None, _) => {
            return stacks::toolchain_in(app).map_or_else(
                || {
                    if static_site(app) {
                        (true, "Nothing to build: a static site.".into())
                    } else {
                        (false, missing(app, place, format!("No project to build found in {place} yet.")))
                    }
                },
                |tool| own_build(app, tool, stop, say),
            );
        }
        _ => return (false, missing(app, place, format!("No project to build found in {place} yet."))),
    };
    match result {
        Ok((true, _)) => (true, "Build succeeded.".into()),
        Ok((false, out)) => (false, format!("Build failed: {}", error_lines(&out))),
        Err(e) => (false, e),
    }
}

/// The engine whose own build runs: the stack's, unless app/ holds another kind of project
/// rather than the engine's. What was built decides, since the person may have asked for
/// something other than the engine picked at the start.
fn engine_for(stack: &str, app: &Path) -> Option<game::Builder> {
    if static_site(app) {
        return None;
    }
    let own_project = stacks::toolchain_in(app).is_some();
    game::Builder::from_stack(stack).filter(|b| !own_project || engine_project(*b, app))
}

/// Whether `app` is a static site with nothing to build: a page with something in it, as the
/// preview finds it, and no game engine's project beside it. A site's `assets/` folder is no sign
/// of Unity (file names ignore case on Windows and macOS), so Unity needs its settings folder too.
fn static_site(app: &Path) -> bool {
    let engine = app.join("project.godot").is_file() || uproject(app).is_some() || (app.join("Assets").is_dir() && app.join("ProjectSettings").is_dir());
    let page = |site: PathBuf| std::fs::read(site.join("index.html")).is_ok_and(|html| html.iter().any(|b| !b.is_ascii_whitespace()));
    !engine && site_dir(app).is_some_and(page)
}

/// A missing-project note, saying where a project is when one was made a folder down (`cargo new
/// app` run inside app/ makes app/app/), so the builder can move it instead of starting again.
/// Only app/, which is the build's own, is told the files belong in it: a person's own project
/// folder, edited in place, may hold projects of its own below it.
fn missing(app: &Path, place: &str, none: String) -> String {
    let Some((dir, manifest)) = nested_project(app) else { return none };
    if place.ends_with('/') {
        format!("{none} There's a {manifest} one folder down, in {place}{dir}/, but the build runs in {place} itself: its files belong there.")
    } else {
        format!("{none} There's a {manifest} one folder down, in {dir}/ in {place}; the build runs in {place} itself.")
    }
}

/// The first folder directly inside `app` (by name, of the first 256 entries) that holds a cargo or
/// npm project, and that project's manifest.
fn nested_project(app: &Path) -> Option<(String, &'static str)> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(app).ok()?.flatten().take(256).map(|e| e.path()).filter(|p| p.is_dir()).collect();
    dirs.sort();
    dirs.iter().find_map(|d| {
        let manifest = ["Cargo.toml", "package.json"].into_iter().find(|m| d.join(m).is_file())?;
        Some((d.file_name()?.to_string_lossy().into_owned(), manifest))
    })
}

/// The Unreal project file in `app`, if there is one.
fn uproject(app: &Path) -> Option<PathBuf> {
    std::fs::read_dir(app).into_iter().flatten().flatten().map(|e| e.path()).find(|p| p.extension().is_some_and(|x| x == "uproject"))
}

/// Whether `app` holds a project of the engine `b` builds with (Rust's is an ordinary cargo one).
fn engine_project(b: game::Builder, app: &Path) -> bool {
    match b {
        game::Builder::Unity => app.join("Assets").is_dir(),
        game::Builder::Unreal => uproject(app).is_some(),
        game::Builder::Godot => app.join("project.godot").is_file(),
        game::Builder::Rust => false,
    }
}

/// The lines that explain a failed build: error lines first, else the last lines.
pub(super) fn error_lines(out: &str) -> String {
    let lines: Vec<&str> = out.lines().filter(|l| !l.trim().is_empty()).collect();
    let errors: Vec<&str> = lines.iter().copied().filter(|l| l.to_ascii_lowercase().contains("error") || l.contains("licen")).take(12).collect();
    if errors.is_empty() { lines[lines.len().saturating_sub(12)..].join("\n") } else { errors.join("\n") }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in toolchain whose build and tests are this shell line.
    #[cfg(unix)]
    fn stand_in(line: &str, no_tests: &[&str]) -> stacks::Toolchain {
        let command = ["sh", "-c", line].map(String::from).to_vec();
        stacks::Toolchain { build: command.clone(), test: command, no_tests: no_tests.iter().map(|s| s.to_string()).collect(), ..Default::default() }
    }

    #[cfg(unix)]
    #[test]
    fn a_persons_own_tests_decide_and_no_tests_is_no_failure() {
        let (d, stop) = (tempfile::tempdir().unwrap(), CancelToken::default());
        let tests = |tool: &stacks::Toolchain| own_tests_check(d.path(), Some(tool), &stop);
        assert!(tests(&stand_in("exit 0", &[])).0);
        let (passed, note) = tests(&stand_in("echo 'adds: 4 is not 5'; echo; echo '1 failed'; exit 1", &[]));
        assert!(!passed && note.ends_with("adds: 4 is not 5\n1 failed"), "the summary, not only lines saying error: {note}");
        let (passed, note) = tests(&stand_in("echo no tests here; exit 5", &["no tests here"]));
        assert!(passed && note.starts_with("No tests ran"), "{note}");
        let missing = stacks::Toolchain { test: vec!["no-such-program-here".into()], ..Default::default() };
        let (passed, note) = tests(&missing);
        assert!(!passed && note.starts_with("Couldn't run the tests"), "{note}");
        let (passed, note) = own_tests_check(d.path(), None, &stop);
        assert!(passed && note.starts_with("No tests ran"), "{note}");
    }

    #[cfg(unix)]
    #[test]
    fn a_projects_own_copy_of_the_program_comes_first() {
        let (d, stop) = (tempfile::tempdir().unwrap(), CancelToken::default());
        std::os::unix::fs::symlink("/bin/sh", d.path().join("own-sh")).unwrap();
        std::fs::write(d.path().join("own-sh.bat"), "not for this system").unwrap();
        let test = ["no-such-program-here", "-c", "echo 'error: from its own copy'; exit 1"].map(String::from).to_vec();
        let tool = stacks::Toolchain { test, local_programs: vec!["not-here".into(), "own-sh.bat".into(), "own-sh".into()], ..Default::default() };
        let (passed, note) = own_tests_check(d.path(), Some(&tool), &stop);
        assert!(!passed && note.contains("from its own copy"), "{note}");
    }

    /// The table's own words, on what the real make (3.81; 4.x quotes alike) and npm (11) print.
    #[cfg(unix)]
    #[test]
    fn only_the_tests_own_absence_means_no_tests() {
        let (d, stop) = (tempfile::tempdir().unwrap(), CancelToken::default());
        let named_by = |file: &str| {
            let f = tempfile::tempdir().unwrap();
            std::fs::write(f.path().join(file), "").unwrap();
            stacks::toolchain_in(f.path()).expect("the table has it")
        };
        let printing = |tool: &stacks::Toolchain, line: &str| {
            let test = ["sh", "-c", "printf '%s\\n' \"$1\"; exit 2", "sh", line].map(String::from).to_vec();
            own_tests_check(d.path(), Some(&stacks::Toolchain { test, ..tool.clone() }), &stop).0
        };
        let (make, node) = (named_by("Makefile"), named_by("package.json"));
        assert!(printing(make, "make: *** No rule to make target `test'.  Stop."));
        assert!(printing(make, "make: *** No rule to make target 'test'.  Stop."));
        assert!(!printing(make, "make: *** No rule to make target `fixture.txt', needed by `test'.  Stop."), "a prerequisite is a failure");
        assert!(!printing(make, "make: *** No rule to make target 'fixture.txt', needed by 'test'.  Stop."));
        assert!(!printing(make, "make[1]: *** No rule to make target `test'.  Stop."), "a make it runs is a failure");
        std::fs::write(d.path().join("package.json"), r#"{"scripts":{"test":"npm test --workspaces"}}"#).unwrap();
        assert!(!printing(node, "npm error Missing script: \"test\""), "one package of several, maybe beside a real failure");
        assert!(!printing(node, "npm error Missing script: \"verify\""), "a script its tests call is a failure");
    }

    #[test]
    fn a_node_project_says_in_its_package_json_whether_it_has_tests() {
        let (d, stop) = (tempfile::tempdir().unwrap(), CancelToken::default());
        std::fs::write(d.path().join("package.json"), "").unwrap();
        let table = stacks::toolchain_in(d.path()).expect("the table has Node.js");
        // Without the Windows name (npm.cmd), which would start the real npm instead of the stand-in.
        let node = stacks::Toolchain { test: vec!["no-such-program-here".into()], windows_program: None, ..table.clone() };
        for json in [r#"{"name":"x"}"#, r#"{"scripts":{"test":"echo \"Error: no test specified\" && exit 1"}}"#] {
            std::fs::write(d.path().join("package.json"), json).unwrap();
            let (passed, note) = own_tests_check(d.path(), Some(&node), &stop);
            assert!(passed && note.starts_with("No tests ran"), "nothing is started: {note}");
        }
        let real = r#"{"scripts":{"test":"jest && echo 'no test specified for integration'"}}"#;
        std::fs::write(d.path().join("package.json"), real).unwrap();
        let (passed, note) = own_tests_check(d.path(), Some(&node), &stop);
        assert!(!passed && note.starts_with("Couldn't run the tests"), "it's started: {note}");
    }

    #[test]
    fn a_persons_own_unity_project_is_opened_not_built_with_our_script() {
        let args = own_unity_args(Path::new("/x/game"));
        assert!(args.iter().any(|a| a == "-batchmode") && args.iter().any(|a| a == "/x/game"), "{args:?}");
        assert!(!args.iter().any(|a| a == "-executeMethod" || a.contains("CraftBuild")), "{args:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_persons_own_build_is_judged_by_its_toolchain() {
        let (d, stop) = (tempfile::tempdir().unwrap(), CancelToken::default());
        assert!(own_build(d.path(), &stand_in("exit 0", &[]), &stop, &quietly).0);
        let (passed, note) = own_build(d.path(), &stand_in("echo 'error: no such file'; exit 2", &[]), &stop, &quietly);
        assert!(!passed && note.contains("no such file"), "{note}");
    }

    /// A `build` script that starts a dev server or watcher would be waited on until the time
    /// limit; it's named instead, and nothing runs.
    #[test]
    fn a_build_script_that_never_exits_is_not_started() {
        for script in [
            "vite",
            "npx vite",
            "vite --port 3000",
            "next dev",
            "npx next dev -p 4000",
            "serve dist",
            "nodemon server.js",
            "tsc --watch",
            "webpack --watch",
            "npm run dev",
            "cross-env NODE_ENV=development vite",
            "tsc && vite",
        ] {
            assert!(script_never_exits(script), "{script}");
        }
        for script in [
            "vite build",
            "npx vite build",
            "tsc && vite build",
            "next build",
            "tsc -b",
            "webpack --mode production",
            "npm run lint && npm run build:prod",
            "rollup -c",
            "cross-env NODE_ENV=production vite build",
            "echo nothing to build",
        ] {
            assert!(!script_never_exits(script), "{script}");
        }
        assert_eq!(npm_run_script(&["npm".into(), "run".into(), "build".into(), "--if-present".into()]), Some("build"));
        assert_eq!(npm_run_script(&["npm.cmd".into(), "run".into(), "build".into()]), Some("build"));
        assert_eq!(npm_run_script(&["cargo".into(), "build".into()]), None);
        let (d, stop) = (tempfile::tempdir().unwrap(), CancelToken::default());
        let app = d.path();
        std::fs::write(app.join("package.json"), r#"{"scripts":{"build":"vite"}}"#).unwrap();
        let (passed, note) = build_check(app, "app/", "Builder's choice", None, &stop);
        assert!(!passed && note.contains("`build` script is `vite`") && note.contains("would never exit"), "{note}");
        // A person's own Node project with the same script.
        let node = stacks::toolchain_in(app).expect("the table has Node.js");
        let (passed, note) = own_build(app, node, &stop, &quietly);
        assert!(!passed && note.contains("would never exit"), "{note}");
        assert_eq!(still_building_line(Duration::from_secs(7 * 60 + 5), "cargo"), "Still building with cargo: 7 min so far…");
    }

    #[test]
    fn a_persons_own_project_with_nothing_to_build_says_so() {
        let stop = CancelToken::default();
        let notes = tempfile::tempdir().unwrap();
        std::fs::write(notes.path().join("notes.md"), "ideas").unwrap();
        let (passed, note) = own_build_check(notes.path(), "the project folder", stacks::BUILDERS_CHOICE, None, &stop);
        assert!(passed && note.starts_with("Nothing was built"), "{note}");
        let site = tempfile::tempdir().unwrap();
        std::fs::write(site.path().join("index.html"), "<p>hi</p>").unwrap();
        let (passed, note) = own_build_check(site.path(), "the project folder", "Plain HTML and CSS", None, &stop);
        assert!(passed && note.contains("static site"), "{note}");
    }

    #[test]
    fn the_build_follows_what_was_built_not_the_stack_picked_first() {
        let d = tempfile::tempdir().unwrap();
        let app = d.path();
        // nothing built yet: the engine's build says what's missing
        assert_eq!(engine_for("Unity", app), Some(game::Builder::Unity));
        // the person asked for something else and got it: npm or cargo builds it
        std::fs::write(app.join("package.json"), "{}").unwrap();
        assert_eq!(engine_for("Unity", app), None);
        assert_eq!(engine_for("Godot", app), None);
        assert_eq!(engine_for("Unreal Engine", app), None);
        // an engine's own project is built by the engine, whatever else is beside it
        std::fs::create_dir(app.join("Assets")).unwrap();
        assert_eq!(engine_for("Unity", app), Some(game::Builder::Unity));
        std::fs::write(app.join("project.godot"), "").unwrap();
        assert_eq!(engine_for("Godot", app), Some(game::Builder::Godot));
        std::fs::write(app.join("Game.uproject"), "{}").unwrap();
        assert_eq!(engine_for("Unreal Engine", app), Some(game::Builder::Unreal));
        // stacks without an engine of their own
        assert_eq!(engine_for("Electron", app), None);
        std::fs::write(app.join("Cargo.toml"), "").unwrap();
        assert_eq!(engine_for("Rust + macroquad", app), None);
    }

    #[test]
    fn a_project_one_folder_down_is_named() {
        let d = tempfile::tempdir().unwrap();
        let app = d.path();
        let stop = CancelToken::default();
        let check = |place| build_check(app, place, "Rust + egui (eframe)", None, &stop);
        assert_eq!(check("app/"), (false, "No project to build found in app/ yet.".into()), "nothing anywhere");
        std::fs::create_dir(app.join("notes")).unwrap();
        assert_eq!(check("app/").1, "No project to build found in app/ yet.", "a folder without a project");
        // What `cargo new app` run inside app/ leaves.
        std::fs::create_dir_all(app.join("app/src")).unwrap();
        std::fs::write(app.join("app/Cargo.toml"), "[package]").unwrap();
        let (ok, note) = check("app/");
        assert!(!ok);
        assert_eq!(
            note,
            "No project to build found in app/ yet. There's a Cargo.toml one folder down, in app/app/, but the build runs in app/ itself: its files belong there."
        );
        // A person's own folder may hold projects below it: where, but no move.
        assert_eq!(
            check("the project folder").1,
            "No project to build found in the project folder yet. There's a Cargo.toml one folder down, in app/ in the project folder; the build runs in the project folder itself."
        );
        // The engines' own missing-project notes say it too.
        let unity = Path::new("C:/Engines/Unity/Editor/Unity.exe");
        let note = build_check(app, "app/", "Unity", Some(unity), &stop).1;
        assert!(note.starts_with("No Unity project found in app/ yet. There's a Cargo.toml one folder down, in app/app/"), "{note}");
    }

    #[test]
    fn a_static_site_has_nothing_to_build() {
        let d = tempfile::tempdir().unwrap();
        let app = d.path();
        let stop = CancelToken::default();
        let passed = (true, "Nothing to build: a static site.".to_string());
        let check = |stack, editor| build_check(app, "app/", stack, editor, &stop);
        assert!(!check("Plain HTML and CSS", None).0, "nothing there yet");

        // #496: the page itself, with no manifest.
        std::fs::write(app.join("index.html"), "<!doctype html><h1>Hello</h1>").unwrap();
        assert_eq!(check("Plain HTML and CSS", None), passed);
        // What was built decides: a page made under a game engine's stack isn't a missing project.
        let unity = Path::new("C:/Engines/Unity/Editor/Unity.exe");
        assert_eq!(check("Unity", Some(unity)), passed);

        // A site's own assets folder doesn't make it a Unity project.
        std::fs::create_dir(app.join("Assets")).unwrap();
        assert_eq!(check("Plain HTML and CSS", None), passed);
        assert_eq!(check("Unity", Some(unity)), passed);

        // A real engine project doesn't pass for the page beside it, with or without the editor,
        // and whichever engine the stack named.
        std::fs::create_dir(app.join("ProjectSettings")).unwrap();
        assert!(!check("Unity", None).0);
        assert!(!check("Plain HTML and CSS", None).0);
        std::fs::remove_dir(app.join("ProjectSettings")).unwrap();
        for project in ["project.godot", "Game.uproject"] {
            std::fs::write(app.join(project), "").unwrap();
            assert!(!check("Unity", None).0, "{project}");
            std::fs::remove_file(app.join(project)).unwrap();
        }

        // A blank page isn't a site, and doesn't pass for one under an engine's stack either.
        std::fs::write(app.join("index.html"), " \n").unwrap();
        assert!(!check("Plain HTML and CSS", None).0);
        std::fs::remove_dir(app.join("Assets")).unwrap();
        assert_eq!(check("Unity", Some(unity)), (false, "No Unity project found in app/ yet.".into()));

        // Already-built pages count too, as they do for the preview.
        let built = tempfile::tempdir().unwrap();
        std::fs::create_dir(built.path().join("dist")).unwrap();
        std::fs::write(built.path().join("dist").join("index.html"), "<h1>Hello</h1>").unwrap();
        assert_eq!(build_check(built.path(), "app/", "Builder's choice", None, &stop), passed);
    }
}
