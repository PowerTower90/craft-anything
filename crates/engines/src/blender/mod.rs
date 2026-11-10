//! Headless Blender for any agent: run a Python script with the project folder as the only
//! writable place ([`run_script`]), read a model's numbers ([`inspect`]), and render previews of
//! many models in a few Blender runs ([`render_previews`]).
//!
//! Every run goes through [`GUARD_PY`]: a Python audit hook and a check on Blender's own writers
//! keep writes inside the allowed folders, refuse starting programs, and refuse
//! `read_factory_settings` (it drops the person's add-ons and GPU setup). Blender itself starts
//! with the person's own settings: never `--factory-startup` here.

use crate::local_gen::find_blender;
use craft_anything_evidence::{ErrorCode, ToolError};
use craft_anything_platform::cancel::CancelToken;
use craft_anything_platform::process::{Process, output_excerpt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// The guard every script runs inside.
pub const GUARD_PY: &str = include_str!("scripts/guard.py");
/// The model helpers (inspect, render).
pub const MODEL_TOOLS_PY: &str = include_str!("scripts/model_tools.py");
/// The helpers every agent script has (OUT, clear_scene, frame_camera, render_preview, …).
pub const PREAMBLE_PY: &str = include_str!("scripts/preamble.py");

/// How long a script may run when nobody says.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// How long reading one model may take.
const INSPECT_TIMEOUT: Duration = Duration::from_secs(3 * 60);
/// Models rendered in one Blender run, and how long each may take.
pub const RENDER_CHUNK: usize = 200;
const PER_MODEL: Duration = Duration::from_secs(45);
/// Paths longer than this are copied to a short one before Blender reads them.
const LONG_PATH: usize = 200;
/// What a preview is: its side in pixels.
pub const PREVIEW_SIDE: u32 = 256;
/// Model files Blender reads here.
pub const MODEL_EXTS: &[&str] = &["glb", "gltf", "fbx", "obj", "stl", "ply", "blend", "dae"];

/// Where Blender and the helper scripts are.
#[derive(Debug, Clone, Default)]
pub struct Setup {
    /// Blender, when it's set; else found on the search path or where it's usually installed.
    pub blender: Option<PathBuf>,
    /// Where the helper scripts and job files are written.
    pub scripts_dir: PathBuf,
}

impl Setup {
    fn blender(&self) -> Result<PathBuf, ToolError> {
        find_blender(self.blender.as_deref()).ok_or_else(|| {
            ToolError::new(ErrorCode::EngineMissing, "Blender wasn't found.")
                .with_remediation("Install Blender, or set its path in Settings → Local generation.")
        })
    }

    fn write(&self, name: &str, text: &str) -> Result<PathBuf, ToolError> {
        std::fs::create_dir_all(&self.scripts_dir).map_err(|e| ToolError::io(self.scripts_dir.display(), e))?;
        let p = self.scripts_dir.join(name);
        if std::fs::read_to_string(&p).ok().as_deref() != Some(text) {
            std::fs::write(&p, text).map_err(|e| ToolError::io(p.display(), e))?;
        }
        Ok(p)
    }

    /// A job file of its own, removed when dropped.
    fn job(&self, what: &str, body: &Value) -> Result<TempFile, ToolError> {
        let bytes = serde_json::to_string_pretty(body).map_err(|e| ToolError::internal(e.to_string()))?;
        self.job_text(what, "json", &bytes)
    }

    fn job_text(&self, what: &str, ext: &str, text: &str) -> Result<TempFile, ToolError> {
        static N: AtomicUsize = AtomicUsize::new(0);
        let p = self.scripts_dir.join(format!("{what}_{}_{}.{ext}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&self.scripts_dir).map_err(|e| ToolError::io(self.scripts_dir.display(), e))?;
        std::fs::write(&p, text).map_err(|e| ToolError::io(p.display(), e))?;
        Ok(TempFile(p))
    }
}

struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Hears each line a run prints.
pub type LineSink = Arc<dyn Fn(&str) + Send + Sync>;

/// What a script run gave.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunResult {
    /// Blender's exit code (1 when the script raised).
    pub exit_code: Option<i32>,
    pub ok: bool,
    /// The last line the script printed after `RESULT `, read as JSON.
    pub result: Option<Value>,
    pub stdout_tail: String,
    pub stderr_tail: String,
    pub seconds: f64,
}

/// The last `RESULT {json}` line in `text`.
pub fn result_line(text: &str) -> Option<Value> {
    text.lines().rev().find_map(|l| l.trim().strip_prefix("RESULT ").and_then(|j| serde_json::from_str(j.trim()).ok()))
}

fn tail(text: &str, max: usize) -> String {
    let t = text.trim_end();
    if t.len() <= max {
        return t.to_string();
    }
    let mut start = t.len() - max;
    while !t.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &t[start..])
}

/// Run `script` (a Python file) in headless Blender with `argv`, writes allowed only inside
/// `writable`. `on_line` hears each line it prints. A script that fails is a result with
/// `ok: false`, not an error; an error is Blender missing, a timeout or a cancel.
#[allow(clippy::too_many_arguments)]
pub fn run_script(
    setup: &Setup,
    script: &Path,
    argv: &[String],
    writable: &[PathBuf],
    timeout: Duration,
    cancel: &CancelToken,
    on_line: Option<LineSink>,
    tick: &dyn Fn(),
) -> Result<RunResult, ToolError> {
    run_job(setup, &json!({"script": script, "argv": argv, "writable": writable}), writable.first(), timeout, cancel, on_line, tick)
}

/// Run a guarded job (see guard.py) in `cwd`.
fn run_job(
    setup: &Setup,
    job: &Value,
    cwd: Option<&PathBuf>,
    timeout: Duration,
    cancel: &CancelToken,
    on_line: Option<LineSink>,
    tick: &dyn Fn(),
) -> Result<RunResult, ToolError> {
    let blender = setup.blender()?;
    let guard = setup.write("blender_guard.py", GUARD_PY)?;
    let job = setup.job("blender_job", job)?;
    let c = cancel.clone();
    let mut p = Process::new(blender)
        .args(["-b", "--python-exit-code", "1", "--python"])
        .arg(&guard)
        .arg("--")
        .arg(&job.0)
        .env("PYTHONUTF8", "1")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .timeout(timeout)
        .cancel_when(move || c.is_cancelled())
        .stdout_cap(4 * 1024 * 1024)
        .label("Blender");
    if let Some(dir) = cwd.filter(|d| d.is_dir()) {
        p = p.cwd(dir);
    }
    if let Some(f) = on_line {
        p = p.on_stdout_line(move |l| f(l));
    }
    let t0 = Instant::now();
    let mut running = p.spawn()?;
    let out = loop {
        if let Some(r) = running.wait_timeout(Duration::from_millis(400)) {
            break r.clone();
        }
        tick();
    }?;
    cancel.check()?;
    let stdout = out.stdout_text();
    Ok(RunResult {
        exit_code: out.code,
        ok: out.success,
        result: result_line(&stdout),
        stdout_tail: tail(&stdout, 4000),
        stderr_tail: tail(&out.stderr_text(), 4000),
        seconds: (t0.elapsed().as_secs_f64() * 10.0).round() / 10.0,
    })
}

/// What an agent's script made: the run, and the files that are new or changed in its folder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentRun {
    #[serde(flatten)]
    pub run: RunResult,
    pub out: PathBuf,
    pub new_files: Vec<PathBuf>,
}

/// Run an agent's Python `code` in headless Blender with the helpers ([`PREAMBLE_PY`]): `out` is
/// its own folder inside `project` (made when missing; relative paths go there), and `project`
/// the only writable place.
pub fn run_agent_script(setup: &Setup, code: &str, project: &Path, out: &Path, timeout: Duration, cancel: &CancelToken) -> Result<AgentRun, ToolError> {
    std::fs::create_dir_all(out).map_err(|e| ToolError::io(out.display(), e))?;
    let preamble = setup.write("blender_preamble.py", PREAMBLE_PY)?;
    let script = setup.job_text("agent_script", "py", code)?;
    let before = files_in(out);
    let started = std::time::SystemTime::now();
    let job = json!({"script": script.0, "argv": [], "writable": [project], "out": out, "preamble": preamble});
    let run = run_job(setup, &job, Some(&out.to_path_buf()), timeout, cancel, None, &|| {})?;
    let new_files = files_in(out).into_iter().filter(|(p, t)| before.iter().all(|(q, u)| q != p || u != t) || *t >= started).map(|(p, _)| p).collect();
    Ok(AgentRun { run, out: out.to_path_buf(), new_files })
}

/// The files under `dir` with when they changed (at most a few thousand).
fn files_in(dir: &Path) -> Vec<(PathBuf, std::time::SystemTime)> {
    let mut out = Vec::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            match e.metadata() {
                Ok(m) if m.is_dir() => todo.push(p),
                Ok(m) => out.push((p, m.modified().unwrap_or(std::time::UNIX_EPOCH))),
                Err(_) => {}
            }
            if out.len() >= 5000 {
                return out;
            }
        }
    }
    out.sort();
    out
}

/// A model's numbers: objects, meshes, faces, triangles, vertices, materials, textures,
/// armatures, bones, animations and bounds.
pub fn inspect(setup: &Setup, model: &Path, cancel: &CancelToken) -> Result<Value, ToolError> {
    let script = setup.write("blender_model_tools.py", MODEL_TOOLS_PY)?;
    let args = vec!["inspect".to_string(), model.display().to_string()];
    let r = run_script(setup, &script, &args, &[], INSPECT_TIMEOUT, cancel, None, &|| {})?;
    r.result.filter(|_| r.ok).ok_or_else(|| {
        ToolError::new(ErrorCode::ProgramFailed, format!("Blender couldn't read {}.", model.display()))
            .with_details(json!({"output": output_excerpt(&format!("{}\n{}", r.stdout_tail, r.stderr_tail))}))
    })
}

/// One model to preview.
#[derive(Debug, Clone, PartialEq)]
pub struct Preview {
    pub id: String,
    pub input: PathBuf,
    pub output: PathBuf,
}

/// What became of one preview.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PreviewResult {
    pub id: String,
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub seconds: f64,
}

/// Render a preview of each model ([`PREVIEW_SIDE`] px, three-quarter view, neutral light,
/// transparent background), [`RENDER_CHUNK`] models per Blender run. `writable` must hold every
/// output. `progress` hears (done, total). One model failing doesn't stop the rest; a Blender
/// run that fails as a whole fails its models with its message.
pub fn render_previews(
    setup: &Setup,
    items: &[Preview],
    writable: &Path,
    cancel: &CancelToken,
    progress: &dyn Fn(usize, usize),
) -> Result<Vec<PreviewResult>, ToolError> {
    let script = setup.write("blender_model_tools.py", MODEL_TOOLS_PY)?;
    let total = items.len();
    let done = Arc::new(AtomicUsize::new(0));
    let mut results: Vec<PreviewResult> = Vec::with_capacity(total);
    for chunk in items.chunks(RENDER_CHUNK) {
        cancel.check()?;
        // Blender's importers can't open a path past Windows' old 260-character limit: a long one
        // is read from a short copy (single-file formats only).
        let short = setup.scripts_dir.join("long-paths");
        let mut copies = Vec::new();
        let list: Vec<Value> = chunk
            .iter()
            .enumerate()
            .map(|(n, p)| {
                let ext = p.input.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
                let long = p.input.as_os_str().len() > LONG_PATH && matches!(ext.as_str(), "obj" | "fbx" | "glb" | "stl" | "ply");
                let input = if long {
                    let to = short.join(format!("{n}.{ext}"));
                    let copied = std::fs::create_dir_all(&short).and_then(|_| std::fs::copy(&p.input, &to));
                    if copied.is_ok() {
                        copies.push(to.clone());
                        to
                    } else {
                        p.input.clone()
                    }
                } else {
                    p.input.clone()
                };
                json!({"id": p.id, "input": input, "output": p.output})
            })
            .collect();
        let job = setup.job("blender_render", &json!({"items": list, "size": PREVIEW_SIDE}))?;
        let d = done.clone();
        let seen = Arc::new(craft_anything_platform::sync::Mutex::new(Vec::<PreviewResult>::new()));
        let s = seen.clone();
        let on_line: LineSink = Arc::new(move |l: &str| {
            if let Some(r) = l.trim().strip_prefix("PREVIEW ").and_then(|j| serde_json::from_str::<PreviewResult>(j).ok()) {
                d.fetch_add(1, Ordering::Relaxed);
                if let Ok(mut v) = s.lock() {
                    v.push(r);
                }
            }
        });
        let timeout = Duration::from_secs(60) + PER_MODEL * u32::try_from(chunk.len()).unwrap_or(u32::MAX);
        let args = vec!["render".to_string(), job.0.display().to_string()];
        let run =
            run_script(setup, &script, &args, &[writable.to_path_buf()], timeout, cancel, Some(on_line), &|| progress(done.load(Ordering::Relaxed), total));
        for c in copies {
            let _ = std::fs::remove_file(c);
        }
        let said = seen.lock().map(|v| v.clone()).unwrap_or_default();
        let whole_failure = match &run {
            Err(e) if e.code == ErrorCode::Cancelled => return Err(e.clone()),
            Err(e) if e.code == ErrorCode::EngineMissing => return Err(e.clone()),
            Err(e) => Some(e.for_people()),
            Ok(r) if r.result.is_none() => Some(format!("Blender stopped early: {}", r.stderr_tail.lines().last().unwrap_or("no reason given"))),
            Ok(_) => None,
        };
        for p in chunk {
            let r = said.iter().find(|r| r.id == p.id).cloned().unwrap_or_else(|| PreviewResult {
                id: p.id.clone(),
                ok: false,
                error: Some(whole_failure.clone().unwrap_or_else(|| "Blender didn't render it.".into())),
                seconds: 0.0,
            });
            results.push(PreviewResult { ok: r.ok && p.output.is_file(), ..r });
        }
        done.store(results.len(), Ordering::Relaxed);
        progress(results.len(), total);
    }
    Ok(results)
}

/// Whether Blender reads `path` as a model here.
pub fn is_model_file(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| MODEL_EXTS.contains(&e.to_ascii_lowercase().as_str()))
}

#[cfg(test)]
mod tests;
