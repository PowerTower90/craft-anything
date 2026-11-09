//! The desktop app's settings come from config.toml and go back to it: loaded the way the app
//! starts (Session::start_in, then CraftApp::new), and written when a field changes in Settings.

use craft_anything_ai::AiHub;
use craft_anything_engine::{Engine, Mode, Session, Settings};
use craft_anything_ui_egui::CraftApp;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

const CONFIG: &str = r#"version = 3
mode = "beginner"
deep_mode = true
theme = "studio"
language = ""
first_run_done = true
ask_before_running = true
auto_mode = true
runtime_study = false
recent = []
project_folders = ['C:\tower power']
[engines.ghidra]
[engines.tools]
[ai]
fallback_chain = []
[[ai.backends]]
enabled = true
kind = "agent_cli"
model = "claude-code"
name = "Claude Code"
[ai.prices]
[ai.roles]
[assets]
budget_usd = 5.0
tripo_credits_per_model = 40
[plugins]
mcp = []
[build]
good_enough = 90.0
max_rounds = 6
web_widths = [1440, 768, 390]
max_pages = 4
parallel = 2
[local_gen]
picture_url = "http://127.0.0.1:9"
shape_url = "http://127.0.0.1:9"
audio_url = "http://127.0.0.1:9"
picture_start = 'C:\games\scripts\start_comfy.ps1'
shape_start = 'C:\games\scripts\start_comfy_amd.ps1'
bg_python = 'C:\games\tools\bgremove\Scripts\python.exe'
blender = 'C:\Program Files\Blender Foundation\Blender 5.2\blender.exe'
"#;

/// The app as main.rs starts it, over the settings in `dir`, showing Settings → Engines.
fn start(dir: &Path) -> Harness<'static, CraftApp> {
    let engine = Engine::shared(Session::start_in(dir), craft_anything_ai::tools());
    let keys = craft_anything_ai::KeyStore::file_only(dir.join("keys.toml"));
    let hub = Arc::new(AiHub::with_stores(engine.clone(), dir.join("config.toml"), keys));
    let mut app = CraftApp::new(engine, hub);
    app.headless = true;
    app.state.settings_open = true;
    app.state.settings_tab = craft_anything_ui_egui::state::SettingsTab::Engines;
    Harness::builder().with_size(egui::vec2(1280.0, 1000.0)).with_max_steps(32).build_eframe(move |cc| {
        CraftApp::setup_context(&cc.egui_ctx);
        app
    })
}

fn run_for(h: &mut Harness<'static, CraftApp>, how_long: Duration, done: impl Fn(&CraftApp) -> bool) -> bool {
    let t0 = Instant::now();
    while t0.elapsed() < how_long {
        h.step();
        if done(h.state()) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

/// The text in the field named `name` (the row's label has the same name).
fn field(h: &Harness<'static, CraftApp>, name: &str) -> Option<String> {
    h.query_all_by_label(name).filter_map(|n| n.value()).find(|v| v != name)
}

#[test]
fn the_app_shows_and_keeps_the_local_generation_paths_in_config_toml() {
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("config.toml");
    std::fs::write(&path, CONFIG).unwrap();
    let mut h = start(d.path());
    assert_eq!(h.state().settings.mode, Mode::Beginner);
    let lg = &h.state().settings.local_gen;
    assert_eq!(lg.picture_start.as_deref(), Some(Path::new(r"C:\games\scripts\start_comfy.ps1")));
    assert_eq!(lg.bg_python.as_deref(), Some(Path::new(r"C:\games\tools\bgremove\Scripts\python.exe")));
    run_for(&mut h, Duration::from_secs(2), |a| a.local_gen.status.is_some());
    h.get_by_label("Local generation").scroll_to_me();
    h.run_steps(2);
    assert_eq!(field(&h, "Start the picture server").as_deref(), Some(r"C:\games\scripts\start_comfy.ps1"));
    assert_eq!(field(&h, "Background removal").as_deref(), Some(r"C:\games\tools\bgremove\Scripts\python.exe"));
    assert_eq!(field(&h, "Blender").as_deref(), Some(r"C:\Program Files\Blender Foundation\Blender 5.2\blender.exe"));

    // A field changed in Settings is written to that config.toml a moment later.
    let before = std::fs::read_to_string(&path).unwrap();
    h.get_by_label("3D server address").focus();
    h.run_steps(2);
    h.get_by_label("3D server address").type_text("0");
    let written = |p: &Path| Settings::load_from(p).local_gen.shape_url == "http://127.0.0.1:90";
    assert!(run_for(&mut h, Duration::from_secs(10), |_| written(&path)), "config.toml has the new address");
    let after = Settings::load_from(&path);
    assert_ne!(std::fs::read_to_string(&path).unwrap(), before);
    assert_eq!(after.local_gen.picture_start.as_deref(), Some(Path::new(r"C:\games\scripts\start_comfy.ps1")), "the rest is kept");
    assert_eq!(after.mode, Mode::Beginner);
    assert_eq!(after.project_folders, [std::path::PathBuf::from(r"C:\tower power")]);
    assert!(!d.path().join("config.pending.toml").exists());
}
