//! A stand-in ComfyUI server on a local port: it takes jobs, answers /history and /queue as the
//! test says (done, failed, or stuck), and serves made-up files. The real servers are never called.

use super::*;
use craft_anything_platform::sync::Mutex;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::Arc;

/// How the stand-in treats each job it's given, in order.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Job {
    Done,
    Fails,
    Stuck,
}

struct Fake {
    jobs: Vec<Job>,
    queued: Vec<String>,
    uploads: Vec<String>,
    /// History polls per job before it's done.
    polls: std::collections::HashMap<String, u32>,
}

/// Start a stand-in server; its address and what it saw.
fn fake_comfy(jobs: Vec<Job>, models: Value) -> (String, Arc<Mutex<Fake>>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let state = Arc::new(Mutex::new(Fake { jobs, queued: Vec::new(), uploads: Vec::new(), polls: Default::default() }));
    let st = state.clone();
    std::thread::spawn(move || {
        for stream in l.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut r = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            if r.read_line(&mut first).is_err() {
                continue;
            }
            let mut len = 0usize;
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; len];
            let _ = r.read_exact(&mut body);
            let path = first.split_whitespace().nth(1).unwrap_or("/").to_string();
            let (status, ctype, out): (u16, &str, Vec<u8>) = {
                let mut s = st.lock().unwrap();
                if path == "/system_stats" {
                    (200, "application/json", b"{\"system\":{}}".to_vec())
                } else if let Some(folder) = path.strip_prefix("/models/") {
                    (200, "application/json", models[folder].to_string().into_bytes())
                } else if path == "/prompt" {
                    let n = s.queued.len();
                    let id = format!("p{n}");
                    s.queued.push(String::from_utf8_lossy(&body).into_owned());
                    (200, "application/json", json!({"prompt_id": id, "number": n}).to_string().into_bytes())
                } else if path == "/upload/image" {
                    s.uploads.push(String::from_utf8_lossy(&body).chars().take(200).collect());
                    (200, "application/json", json!({"name": "uploaded.png", "subfolder": "", "type": "input"}).to_string().into_bytes())
                } else if let Some(id) = path.strip_prefix("/history/") {
                    let n: usize = id.trim_start_matches('p').parse().unwrap_or(0);
                    let job = s.jobs.get(n).copied().unwrap_or(Job::Done);
                    let polls = {
                        let p = s.polls.entry(id.to_string()).or_insert(0);
                        *p += 1;
                        *p
                    };
                    let graph: Value = s.queued.get(n).and_then(|q| serde_json::from_str(q).ok()).unwrap_or(Value::Null);
                    let shape = graph["prompt"][comfy::SHAPE_OUTPUT_NODE]["class_type"] == "SaveGLB";
                    let v = match job {
                        Job::Stuck => json!({}),
                        _ if polls < 2 => json!({}),
                        Job::Fails => json!({id: {"status": {"status_str": "error", "completed": false,
                            "messages": [["execution_error", {"exception_message": "CUDA out of memory", "node_type": "KSampler"}]]}, "outputs": {}}}),
                        Job::Done if shape => json!({id: {"status": {"status_str": "success", "completed": true},
                            "outputs": {"10": {"3d": [{"filename": format!("{id}.glb"), "subfolder": "craft", "type": "output"}]}}}}),
                        Job::Done => json!({id: {"status": {"status_str": "success", "completed": true},
                            "outputs": {"10": {"images": [{"filename": format!("{id}.png"), "subfolder": "craft", "type": "output"}]}}}}),
                    };
                    (200, "application/json", v.to_string().into_bytes())
                } else if path == "/queue" {
                    (200, "application/json", json!({"queue_running": [], "queue_pending": []}).to_string().into_bytes())
                } else if path.starts_with("/view?") {
                    let glb = path.contains(".glb");
                    (200, "application/octet-stream", if glb { b"glTF-fake-shape".to_vec() } else { b"\x89PNG-fake-picture".to_vec() })
                } else {
                    (404, "text/plain", b"no".to_vec())
                }
            };
            let head = format!("HTTP/1.1 {status} OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", out.len());
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&out);
        }
    });
    (base, state)
}

fn models() -> Value {
    json!({
        "diffusion_models": [comfy::IMAGE_UNET], "text_encoders": [comfy::IMAGE_TEXT_ENCODER],
        "vae": [comfy::IMAGE_VAE], "checkpoints": [comfy::SHAPE_CHECKPOINT],
    })
}

fn setup(base: &str, dir: &Path) -> Setup {
    Setup {
        picture_url: base.into(),
        shape_url: base.into(),
        scripts_dir: dir.join("scripts"),
        limits: Limits { job: Duration::from_secs(20), stall: Duration::from_millis(600), poll: Duration::from_millis(20) },
        ..Setup::default()
    }
}

/// A stand-in Blender: writes the files the finishing script would and prints its report.
fn fake_blender(dir: &Path) -> PathBuf {
    if cfg!(windows) {
        let p = dir.join("blender.cmd");
        // Arguments: -b --factory-startup --python <script> -- <shape> <ref> <out_base> <faces> <height>
        std::fs::write(
            &p,
            "@echo off\r\necho glb> \"%8.glb\"\r\necho fbx> \"%8.fbx\"\r\necho png> \"%8_preview.png\"\r\necho FINISH {\"faces_out\": 1200, \"size_m\": [1, 1, 1]}\r\n",
        )
        .unwrap();
        p
    } else {
        let p = dir.join("blender.sh");
        std::fs::write(&p, "#!/bin/sh\necho glb > \"$8.glb\"\necho fbx > \"$8.fbx\"\necho png > \"$8_preview.png\"\necho 'FINISH {\"faces_out\": 1200, \"size_m\": [1, 1, 1]}'\n")
            .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }
}

#[test]
fn status_says_what_is_ready_and_what_is_missing() {
    let d = tempfile::tempdir().unwrap();
    let (base, _) = fake_comfy(vec![], json!({"diffusion_models": [comfy::IMAGE_UNET], "text_encoders": [], "vae": [comfy::IMAGE_VAE], "checkpoints": []}));
    let mut s = setup(&base, d.path());
    s.shape_url = "http://127.0.0.1:9".into();
    s.shape_start = Some(d.path().join("start.ps1"));
    std::fs::write(d.path().join("start.ps1"), "").unwrap();
    let st = status(&s);
    assert_eq!(st.pictures.state, "missing_model");
    assert!(st.pictures.detail.contains(comfy::IMAGE_TEXT_ENCODER), "{}", st.pictures.detail);
    assert_eq!((st.shapes.state.as_str(), st.shapes.startable), ("not_running", true));
    assert_eq!(st.background_removal.state, "not_set_up");
    let (base, _) = fake_comfy(vec![], models());
    let st = status(&setup(&base, d.path()));
    assert!(st.pictures.ready && st.shapes.ready, "{st:?}");
}

#[test]
fn a_picture_is_queued_waited_for_and_saved() {
    let d = tempfile::tempdir().unwrap();
    let (base, seen) = fake_comfy(vec![Job::Done], models());
    let s = setup(&base, d.path());
    let out = d.path().join("out/icon.png");
    let n = picture(&s, "Game asset: a gear", 1000, 500, 1, &out, &CancelToken::default(), &|_| {}).unwrap();
    assert_eq!(std::fs::read(&out).unwrap(), b"\x89PNG-fake-picture");
    assert_eq!(n, 17);
    let sent: Value = serde_json::from_str(&seen.lock().unwrap().queued[0]).unwrap();
    assert_eq!(sent["prompt"]["5"]["inputs"]["text"], "Game asset: a gear");
    assert_eq!((sent["prompt"]["7"]["inputs"]["width"].as_u64(), sent["prompt"]["7"]["inputs"]["height"].as_u64()), (Some(992), Some(496)), "multiples of 16");
}

#[test]
fn a_stuck_server_and_a_failed_job_say_so_instead_of_hanging() {
    let d = tempfile::tempdir().unwrap();
    let (base, _) = fake_comfy(vec![Job::Stuck, Job::Fails], models());
    let s = setup(&base, d.path());
    let t0 = Instant::now();
    let e = picture(&s, "x", 512, 512, 1, &d.path().join("a.png"), &CancelToken::default(), &|_| {}).unwrap_err();
    assert_eq!(e.code, ErrorCode::TimedOut);
    assert!(e.message.contains("stopped making progress"), "{}", e.message);
    assert!(t0.elapsed() < Duration::from_secs(10));
    let e = picture(&s, "x", 512, 512, 1, &d.path().join("b.png"), &CancelToken::default(), &|_| {}).unwrap_err();
    assert!(e.message.contains("CUDA out of memory (KSampler)"), "{}", e.message);
    // Cancelling stops a wait at once.
    let (base, _) = fake_comfy(vec![Job::Stuck], models());
    let mut s = setup(&base, d.path());
    s.limits.stall = Duration::from_secs(60);
    let c = CancelToken::default();
    let c2 = c.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        c2.cancel();
    });
    let e = picture(&s, "x", 512, 512, 1, &d.path().join("c.png"), &c, &|_| {}).unwrap_err();
    assert_eq!(e.code, ErrorCode::Cancelled);
}

#[test]
fn a_batch_makes_pictures_and_models_and_carries_on_past_a_failure() {
    let d = tempfile::tempdir().unwrap();
    // Jobs in order: the first picture fails; the model's reference, then its shape, succeed; the last picture succeeds.
    let (base, seen) = fake_comfy(vec![Job::Fails, Job::Done, Job::Done, Job::Done], models());
    let mut s = setup(&base, d.path());
    s.blender = Some(fake_blender(d.path()));
    let items = vec![
        BatchItem {
            id: "icon/broken".into(),
            kind: ItemKind::Image,
            description: "a shield".into(),
            width: None,
            height: None,
            transparent: Some(false),
            height_m: None,
            faces: None,
        },
        BatchItem {
            id: "crate".into(),
            kind: ItemKind::Model,
            description: "a wooden crate".into(),
            width: None,
            height: None,
            transparent: None,
            height_m: Some(0.8),
            faces: Some(4000),
        },
        BatchItem {
            id: "gem".into(),
            kind: ItemKind::Image,
            description: "a red gem".into(),
            width: Some(512),
            height: Some(512),
            transparent: Some(false),
            height_m: None,
            faces: None,
        },
    ];
    let out = d.path().join("generated/hud-1");
    let steps = Mutex::new(Vec::<String>::new());
    let results =
        run_batch(&s, &items, "Pixel art", &out, &CancelToken::default(), &|n, total, what| steps.lock().unwrap().push(format!("{n}/{total} {what}")));
    assert_eq!(results.iter().map(|r| (r.id.as_str(), r.ok)).collect::<Vec<_>>(), [("icon/broken", false), ("crate", true), ("gem", true)]);
    assert!(results[0].error.as_deref().unwrap_or_default().contains("out of memory"));
    assert!(out.join("crate.glb").is_file() && out.join("crate.fbx").is_file() && out.join("crate_shape.glb").is_file());
    assert!(out.join("gem.png").is_file());
    assert_eq!(results[1].stats["faces_out"], 1200);
    let fake = seen.lock().unwrap();
    assert_eq!(fake.uploads.len(), 1, "the reference went to the 3D server");
    assert!(fake.queued.iter().filter(|q| !q.contains("SaveGLB")).all(|q| q.contains("Pixel art")), "the style is in every picture prompt");
    assert!(fake.queued[1].contains("3D game asset reference"));
    drop(fake);
    assert!(steps.lock().unwrap().iter().any(|s| s.starts_with("3/3")), "it said how far it got");
    let m = manifest("hud-1", "Pixel art", d.path(), &items, &results);
    assert_eq!((m["made"].as_u64(), m["failed"].as_u64()), (Some(2), Some(1)));
    assert_eq!(m["items"][2]["files"][0], "generated/hud-1/gem.png");
    assert_eq!(m["style"], "Pixel art");
}

#[test]
fn prompts_names_and_seeds() {
    assert_eq!(
        picture_prompt("a pickaxe.", "Pixel art.", true),
        "Game asset: a pickaxe. Pixel art. Centered, single object, isolated on a plain white background, no text, no frame."
    );
    assert_eq!(picture_prompt("cave rock", "", false), "Game art: cave rock. No text, no watermark.");
    assert!(model_prompt("a crate", "Low-poly").contains("front view: a crate. Low-poly."));
    assert_eq!(file_stem("hud/icon 1"), "hud_icon_1");
    assert_eq!(file_stem("///"), "item");
    assert_eq!(seed_for("a"), seed_for("a"));
    assert_ne!(seed_for("a"), seed_for("b"));
}
