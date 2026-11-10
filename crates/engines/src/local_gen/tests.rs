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
                } else if let Some(class) = path.strip_prefix("/object_info/") {
                    let has = models["nodes"].as_array().is_some_and(|n| n.iter().any(|n| n == class));
                    (200, "application/json", if has { json!({class: {}}) } else { json!({}) }.to_string().into_bytes())
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
                    let sound = graph["prompt"][audio::AUDIO_OUTPUT_NODE]["class_type"] == "SaveAudio";
                    let better = graph["prompt"][trellis::MODEL_OUTPUT_NODE]["class_type"] == "SaveGLB";
                    let v = match job {
                        Job::Stuck => json!({}),
                        _ if polls < 2 => json!({}),
                        Job::Fails => json!({id: {"status": {"status_str": "error", "completed": false,
                            "messages": [["execution_error", {"exception_message": "CUDA out of memory", "node_type": "KSampler"}]]}, "outputs": {}}}),
                        Job::Done if sound => json!({id: {"status": {"status_str": "success", "completed": true},
                            "outputs": {"20": {"audio": [{"filename": format!("{id}.flac"), "subfolder": "craft", "type": "output"}]}}}}),
                        Job::Done if better => {
                            let mut out = json!({"40": {"3d": [{"filename": format!("{id}.glb"), "subfolder": "craft", "type": "output"}]}});
                            for (node, map) in trellis::MAP_NODES {
                                out[*node] = json!({"images": [{"filename": format!("{id}_{map}.png"), "subfolder": "craft", "type": "output"}]});
                            }
                            json!({id: {"status": {"status_str": "success", "completed": true}, "outputs": out}})
                        }
                        Job::Done if shape => json!({id: {"status": {"status_str": "success", "completed": true},
                            "outputs": {"10": {"3d": [{"filename": format!("{id}.glb"), "subfolder": "craft", "type": "output"}]}}}}),
                        Job::Done => json!({id: {"status": {"status_str": "success", "completed": true},
                            "outputs": {"10": {"images": [{"filename": format!("{id}.png"), "subfolder": "craft", "type": "output"}]}}}}),
                    };
                    (200, "application/json", v.to_string().into_bytes())
                } else if path == "/queue" {
                    (200, "application/json", json!({"queue_running": [], "queue_pending": []}).to_string().into_bytes())
                } else if path.starts_with("/view?") {
                    let bytes = if path.contains(".glb") {
                        b"glTF-fake-shape".to_vec()
                    } else if path.contains(".flac") {
                        include_bytes!("testdata/tiny.flac").to_vec()
                    } else {
                        b"\x89PNG-fake-picture".to_vec()
                    };
                    (200, "application/octet-stream", bytes)
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

/// The picture server with the TRELLIS.2 chain too.
fn better_models() -> Value {
    let mut m = models();
    for (folder, file) in trellis::MODELS {
        m[*folder] = json!(m[*folder].as_array().cloned().unwrap_or_default().into_iter().chain([json!(file)]).collect::<Vec<_>>());
    }
    m["nodes"] = json!(trellis::NODES);
    m
}

fn audio_models() -> Value {
    json!({
        "checkpoints": [audio::SOUND_CHECKPOINT], "diffusion_models": [audio::MUSIC_UNET], "vae": [audio::MUSIC_VAE],
        "text_encoders": [audio::SOUND_TEXT_ENCODER, audio::MUSIC_TEXT_ENCODER_SMALL, audio::MUSIC_TEXT_ENCODER_LARGE],
    })
}

fn setup(base: &str, dir: &Path) -> Setup {
    Setup {
        picture_url: base.into(),
        shape_url: base.into(),
        audio_url: base.into(),
        scripts_dir: dir.join("scripts"),
        limits: Limits { job: Duration::from_secs(20), stall: Duration::from_millis(600), poll: Duration::from_millis(20) },
        ..Setup::default()
    }
}

/// A stand-in Blender: writes the files the finishing script would and prints its report. The
/// quick route's arguments are -b --factory-startup --python <script> -- <shape> <ref> <out_base>
/// <faces> <height>; the better route's -b --python <script> -- <out_base>_finish.json.
fn fake_blender(dir: &Path) -> PathBuf {
    if cfg!(windows) {
        let p = dir.join("blender.cmd");
        std::fs::write(
            &p,
            [
                "@echo off",
                "if \"%4\"==\"--\" goto pbr",
                "echo glb> \"%8.glb\"",
                "echo fbx> \"%8.fbx\"",
                "echo png> \"%8_preview.png\"",
                "echo FINISH {\"faces_out\": 1200, \"size_m\": [1, 1, 1]}",
                "exit /b 0",
                ":pbr",
                "set b=%~dpn5",
                "set b=%b:_finish=%",
                "for %%e in (.glb .fbx _preview.png _BaseColor.png _Normal.png _ORM.png) do echo x> \"%b%%%e\"",
                "echo FINISH {\"faces_out\": 900, \"lod_faces\": [450, 225], \"maps_out\": [\"BaseColor\", \"Normal\", \"ORM\"]}",
                "",
            ]
            .join("\r\n"),
        )
        .unwrap();
        p
    } else {
        let p = dir.join("blender.sh");
        std::fs::write(
            &p,
            [
                "#!/bin/sh",
                "if [ \"$4\" = \"--\" ]; then",
                "  b=\"${5%_finish.json}\"",
                "  for e in .glb .fbx _preview.png _BaseColor.png _Normal.png _ORM.png; do echo x > \"$b$e\"; done",
                "  echo 'FINISH {\"faces_out\": 900, \"lod_faces\": [450, 225], \"maps_out\": [\"BaseColor\", \"Normal\", \"ORM\"]}'",
                "  exit 0",
                "fi",
                "echo glb > \"$8.glb\"",
                "echo fbx > \"$8.fbx\"",
                "echo png > \"$8_preview.png\"",
                "echo 'FINISH {\"faces_out\": 1200, \"size_m\": [1, 1, 1]}'",
                "",
            ]
            .join("\n"),
        )
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
    let mut broken = BatchItem::new("icon/broken", ItemKind::Image, "a shield");
    broken.transparent = Some(false);
    let mut crate_ = BatchItem::new("crate", ItemKind::Model, "a wooden crate");
    crate_.height_m = Some(0.8);
    crate_.faces = Some(4000);
    let mut gem = BatchItem::new("gem", ItemKind::Image, "a red gem");
    (gem.width, gem.height, gem.transparent) = (Some(512), Some(512), Some(false));
    let items = vec![broken, crate_, gem];
    let out = d.path().join("generated/hud-1");
    let steps = Mutex::new(Vec::<String>::new());
    let results = run_batch(&s, &items, "Pixel art", &out, &d.path().join("voices"), &CancelToken::default(), &|n, total, what| {
        steps.lock().unwrap().push(format!("{n}/{total} {what}"))
    });
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

#[test]
fn sounds_and_music_are_made_on_the_audio_server_as_wav() {
    let d = tempfile::tempdir().unwrap();
    let (base, seen) = fake_comfy(vec![Job::Done, Job::Done], audio_models());
    let s = setup(&base, d.path());
    let st = status(&s);
    assert!(st.audio.ready, "{:?}", st.audio);
    assert!(st.sounds_possible());
    let out = d.path().join("out/door.wav");
    let n = audio::sound(&s, SoundKind::Sfx, "a door creaks open", "Dark fantasy", Some(0.3), 1, &out, &CancelToken::default(), &|_| {}).unwrap();
    let wav = std::fs::read(&out).unwrap();
    assert_eq!(n, wav.len() as u64);
    assert_eq!(audio::wav_info(&wav).unwrap().sample_rate, 22050, "the server's FLAC came back as WAV");
    let music = d.path().join("out/theme.wav");
    audio::sound(&s, SoundKind::Music, "slow cave theme with low strings", "Dark fantasy", Some(500.0), 2, &music, &CancelToken::default(), &|_| {}).unwrap();
    let fake = seen.lock().unwrap();
    let sfx: Value = serde_json::from_str(&fake.queued[0]).unwrap();
    assert_eq!(sfx["prompt"]["8"]["inputs"]["duration"], 0.5, "clamped to half a second at least");
    assert!(sfx["prompt"]["3"]["inputs"]["text"].as_str().unwrap().contains("a door creaks open. Dark fantasy."));
    let theme: Value = serde_json::from_str(&fake.queued[1]).unwrap();
    assert_eq!(theme["prompt"]["6"]["inputs"]["seconds"], 240.0, "music is at most four minutes");
    assert_eq!(theme["prompt"]["4"]["inputs"]["lyrics"], "", "instrumental");
    assert!(theme["prompt"]["4"]["inputs"]["tags"].as_str().unwrap().contains("Dark fantasy"), "the style is in the music's tags");
}

#[test]
fn a_stuck_or_failing_audio_server_says_so() {
    let d = tempfile::tempdir().unwrap();
    let (base, _) = fake_comfy(vec![Job::Stuck, Job::Fails], audio_models());
    let s = setup(&base, d.path());
    let t0 = Instant::now();
    let e = audio::sound(&s, SoundKind::Ui, "a click", "", None, 1, &d.path().join("a.wav"), &CancelToken::default(), &|_| {}).unwrap_err();
    assert_eq!(e.code, ErrorCode::TimedOut);
    assert!(t0.elapsed() < Duration::from_secs(10));
    let e = audio::sound(&s, SoundKind::Ambience, "wind", "", None, 1, &d.path().join("b.wav"), &CancelToken::default(), &|_| {}).unwrap_err();
    assert!(e.message.contains("CUDA out of memory"), "{}", e.message);
    assert!(!d.path().join("b.wav").exists());
    // Without the models it isn't ready.
    let (base, _) = fake_comfy(vec![], models());
    let st = status(&setup(&base, d.path()));
    assert_eq!(st.audio.state, "missing_model");
    assert!(st.audio.detail.contains(audio::SOUND_CHECKPOINT), "{}", st.audio.detail);
}

/// A stand-in for the voice Python: a script that does what voice_line.py does with its job
/// (writes the files, prints the JSON line) and logs each job's op. Lines whose text has "FAIL"
/// fail.
fn fake_voice_python(dir: &Path) -> Option<PathBuf> {
    let log = dir.join("voice_ops.txt");
    if cfg!(windows) {
        let ps1 = dir.join("voice.ps1");
        std::fs::write(
            &ps1,
            format!(
                r#"$j = Get-Content -Raw -Encoding utf8 $args[0] | ConvertFrom-Json
Add-Content -Path '{log}' -Value $j.op
if ($j.op -eq 'transcribe') {{ Write-Output '{{"ok": true, "text": "words heard in the original"}}'; exit 0 }}
if ($j.op -eq 'design') {{
  Set-Content -Path $j.out -Value 'RIFF-designed'
  $o = @{{ok = $true; out = $j.out; seconds = 3.5; sample_text = $j.sample_text}}
  Write-Output ($o | ConvertTo-Json -Compress); exit 0
}}
$made = @(); $failed = @()
foreach ($l in $j.lines) {{
  if ($l.text -like '*FAIL*') {{ $failed += @{{out = $l.out; error = 'could not say it'}} }}
  else {{ Set-Content -Path $l.out -Value $l.text; $made += @{{out = $l.out; seconds = 1.25}} }}
}}
$o = @{{ok = $true; made = $made; failed = $failed}}
Write-Output ($o | ConvertTo-Json -Compress -Depth 4)
"#,
                log = log.display()
            ),
        )
        .unwrap();
        let p = dir.join("python.cmd");
        std::fs::write(&p, "@powershell -NoProfile -ExecutionPolicy Bypass -File \"%~dp0voice.ps1\" %3\r\n").unwrap();
        Some(p)
    } else {
        let py = craft_anything_platform::which::which("python3")?;
        let script = dir.join("voice_fake.py");
        std::fs::write(
            &script,
            format!(
                r#"import json, sys
j = json.load(open(sys.argv[1], encoding="utf-8"))
open({log:?}, "a").write(j["op"] + "\n")
if j["op"] == "transcribe":
    print(json.dumps({{"ok": True, "text": "words heard in the original"}})); sys.exit(0)
if j["op"] == "design":
    open(j["out"], "w").write("RIFF-designed")
    print(json.dumps({{"ok": True, "out": j["out"], "seconds": 3.5, "sample_text": j["sample_text"]}})); sys.exit(0)
made, failed = [], []
for l in j["lines"]:
    if "FAIL" in l["text"]:
        failed.append({{"out": l["out"], "error": "could not say it"}})
    else:
        open(l["out"], "w").write(l["text"]); made.append({{"out": l["out"], "seconds": 1.25}})
print(json.dumps({{"ok": True, "made": made, "failed": failed}}))
"#,
                log = log.display().to_string()
            ),
        )
        .unwrap();
        let p = dir.join("python.sh");
        std::fs::write(&p, format!("#!/bin/sh\nexec {} {:?} \"$3\"\n", py.display(), script.display().to_string())).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        Some(p)
    }
}

#[test]
fn each_character_gets_one_designed_voice_and_all_their_lines() {
    let d = tempfile::tempdir().unwrap();
    let Some(python) = fake_voice_python(d.path()) else { return };
    let mut s = setup("http://127.0.0.1:9", d.path());
    s.voice_python = Some(python);
    let voices = d.path().join("voices");
    let original = d.path().join("hello_01.wav");
    std::fs::write(&original, b"RIFF-original").unwrap();
    let line = |id: &str, who: &str, text: Option<&str>| voice::VoiceLine {
        id: id.into(),
        character: who.into(),
        voice_description: format!("{who}: deep, slow, a little hoarse"),
        text: text.map(str::to_string),
        transcribe_from: text.is_none().then(|| original.clone()),
        out: d.path().join(format!("lines/{id}.wav")),
    };
    let lines = [line("a", "Dwarf Miner", Some("We dig at dawn.")), line("b", "Dwarf Miner", None), line("c", "Elf", Some("FAIL this one"))];
    let r = voice::voice_lines(&s, &voices, &lines, &CancelToken::default(), &|_| {}).unwrap();
    assert!(r[0].ok && r[1].ok, "{r:?}");
    assert_eq!(r[1].text.as_deref(), Some("words heard in the original"), "the words came from the original");
    assert_eq!(std::fs::read_to_string(&lines[0].out).unwrap().trim(), "We dig at dawn.");
    assert!(!r[2].ok && r[2].error.as_deref() == Some("could not say it"), "{:?}", r[2]);
    assert!(r[0].designed_voice);
    let card = voice::voice_of(&voices, "Dwarf Miner").unwrap();
    assert_eq!(card.reference_text, voice::REFERENCE_TEXT);
    assert!(card.description.contains("deep, slow"));
    // One transcription, a design per character, one speaking run per character.
    let ops = std::fs::read_to_string(d.path().join("voice_ops.txt")).unwrap();
    let ops: Vec<&str> = ops.lines().map(str::trim).collect();
    assert_eq!(ops, ["transcribe", "design", "speak", "design", "speak"]);
    // The next line of a character with a voice reuses it: no new design.
    let again = voice::voice_lines(&s, &voices, &[line("d", "Dwarf Miner", Some("Back to work."))], &CancelToken::default(), &|_| {}).unwrap();
    assert!(again[0].ok && !again[0].designed_voice);
    assert_eq!(std::fs::read_to_string(d.path().join("voice_ops.txt")).unwrap().lines().count(), 6, "one more speak, no design");
    // Without the voice Python it says so.
    s.voice_python = None;
    let e = voice::voice_lines(&s, &voices, &lines, &CancelToken::default(), &|_| {}).unwrap_err();
    assert_eq!(e.code, ErrorCode::EngineMissing);
}

#[test]
fn a_mixed_batch_uses_every_lane_at_once() {
    let d = tempfile::tempdir().unwrap();
    let (pictures, _) = fake_comfy(vec![Job::Done], models());
    let (sounds, heard) = fake_comfy(vec![Job::Done, Job::Fails], audio_models());
    let mut s = setup(&pictures, d.path());
    s.audio_url = sounds;
    s.voice_python = fake_voice_python(d.path());
    let mut hit = BatchItem::new("sfx/hit", ItemKind::Sound, "a sword hits a wooden shield");
    hit.seconds = Some(1.2);
    let mut wind = BatchItem::new("amb/wind", ItemKind::Sound, "wind in a cave");
    wind.sound = Some(SoundKind::Ambience);
    let mut hello = BatchItem::new("vo/hello", ItemKind::Voice, "a greeting");
    hello.character = Some("Guard".into());
    hello.voice_description = Some("a young guard, bright and quick".into());
    hello.text = Some("Halt! Who goes there?".into());
    let mut nobody = BatchItem::new("vo/nobody", ItemKind::Voice, "no words");
    nobody.character = Some("Guard".into());
    let items = vec![BatchItem::new("icon", ItemKind::Image, "a gem"), hit, wind, hello, nobody];
    let out = d.path().join("generated/mix");
    let r = run_batch(&s, &items, "Sci-fi", &out, &d.path().join("generated/voices"), &CancelToken::default(), &|_, _, _| {});
    let got: Vec<(&str, bool)> = r.iter().map(|r| (r.id.as_str(), r.ok)).collect();
    let voiced = s.voice_python.is_some();
    assert_eq!(got, [("icon", true), ("sfx/hit", true), ("amb/wind", false), ("vo/hello", voiced), ("vo/nobody", false)], "{r:?}");
    assert!(out.join("sfx_hit.wav").is_file());
    assert!(r[2].error.as_deref().unwrap_or_default().contains("out of memory"));
    assert!(r[4].error.as_deref().unwrap_or_default().contains("words"), "{:?}", r[4].error);
    if voiced {
        assert!(out.join("vo_hello.wav").is_file());
        assert_eq!(r[3].stats["text"], "Halt! Who goes there?");
        assert!(d.path().join("generated/voices/Guard.wav").is_file());
    }
    let fake = heard.lock().unwrap();
    assert_eq!(fake.queued.len(), 2, "the sounds went to the audio server");
    assert!(fake.queued.iter().all(|q| q.contains("Sci-fi")), "the style is in the sound prompts");
    let m = manifest("mix", "Sci-fi", d.path(), &items, &r);
    assert_eq!(m["items"][1]["kind"], "sound");
}

#[test]
fn better_models_are_ready_only_with_the_nodes_and_every_model_file() {
    let d = tempfile::tempdir().unwrap();
    let (base, _) = fake_comfy(vec![], models());
    let st = status(&setup(&base, d.path()));
    assert_eq!(st.better_models.state, "missing_model");
    assert!(st.better_models.detail.contains("pixal3d_int8_convrot.safetensors"), "{}", st.better_models.detail);
    let mut no_nodes = better_models();
    no_nodes["nodes"] = json!(["UnwrapMesh"]);
    let (base, _) = fake_comfy(vec![], no_nodes);
    let st = status(&setup(&base, d.path()));
    assert!(!st.better_models.ready && st.better_models.detail.contains("Pixal3DConditioning"), "{:?}", st.better_models);
    let (base, _) = fake_comfy(vec![], better_models());
    let mut s = setup(&base, d.path());
    let st = status(&s);
    assert!(st.better_models.ready, "{:?}", st.better_models);
    s.blender = Some(fake_blender(d.path()));
    assert!(status(&s).better_models_possible());
    // A stopped picture server that can be started counts as possible.
    let mut stopped = setup("http://127.0.0.1:9", d.path());
    stopped.blender = s.blender.clone();
    stopped.picture_start = Some(d.path().join("start.ps1"));
    std::fs::write(d.path().join("start.ps1"), "").unwrap();
    let st = status(&stopped);
    assert_eq!(st.better_models.state, "not_running");
    assert!(st.better_models_possible());
}

#[test]
fn a_model_is_made_the_better_way_when_the_picture_server_can_and_the_quick_way_when_asked() {
    let d = tempfile::tempdir().unwrap();
    // Jobs in order: the dwarf's reference picture and its TRELLIS.2 model, the crate's reference and its quick shape.
    let (base, seen) = fake_comfy(vec![Job::Done; 4], better_models());
    let mut s = setup(&base, d.path());
    s.blender = Some(fake_blender(d.path()));
    let mut dwarf = BatchItem::new("dwarf", ItemKind::Model, "a stocky armoured dwarf miner");
    dwarf.role = Some(ModelRole::Hero);
    dwarf.height_m = Some(1.4);
    let mut crate_ = BatchItem::new("crate", ItemKind::Model, "a wooden crate");
    crate_.model_method = Some(ModelMethod::Quick);
    let out = d.path().join("generated/m");
    let r = run_batch(&s, &[dwarf, crate_], "", &out, &d.path().join("voices"), &CancelToken::default(), &|_, _, _| {});
    assert!(r[0].ok && r[1].ok, "{r:?}");
    assert_eq!(r[0].stats["method"], "better");
    assert_eq!(r[1].stats["method"], "quick");
    assert_eq!(r[0].stats["lod_faces"], json!([450, 225]));
    for f in
        ["dwarf.glb", "dwarf.fbx", "dwarf_preview.png", "dwarf_BaseColor.png", "dwarf_Normal.png", "dwarf_ORM.png", "dwarf_trellis.glb", "dwarf_trellis_ao.png"]
    {
        assert!(out.join(f).is_file(), "{f}");
        assert!(r[0].files.contains(&out.join(f)), "{f} is listed");
    }
    assert_eq!(r[0].files[0], out.join("dwarf.glb"), "the model first");
    assert!(!out.join("dwarf_finish.json").exists(), "the Blender job file is cleaned up");
    let fake = seen.lock().unwrap();
    let sent: Vec<Value> = fake.queued.iter().map(|q| serde_json::from_str(q).unwrap()).collect();
    assert!(
        sent[0]["prompt"]["5"]["inputs"]["text"]
            .as_str()
            .unwrap()
            .contains("three-quarter front view at eye level, facing the viewer: a stocky armoured dwarf miner")
    );
    let better = sent.iter().find(|g| g["prompt"]["40"]["class_type"] == "SaveGLB").expect("a TRELLIS.2 job");
    assert_eq!(better["prompt"]["31"]["inputs"]["target_face_count"], 30_000, "a hero's budget");
    assert_eq!(better["prompt"]["1"]["inputs"]["image"], "uploaded.png");
    assert!(sent.iter().any(|g| g["prompt"][comfy::SHAPE_OUTPUT_NODE]["class_type"] == "SaveGLB"), "the crate went the quick way");
    assert!(sent.iter().any(|g| g["prompt"]["5"]["inputs"]["text"].as_str().is_some_and(|t| t.contains("front view: a wooden crate"))));
}

#[test]
fn asking_for_a_better_model_where_it_can_not_be_made_says_why() {
    let d = tempfile::tempdir().unwrap();
    let (base, seen) = fake_comfy(vec![Job::Done; 2], models());
    let mut s = setup(&base, d.path());
    s.blender = Some(fake_blender(d.path()));
    let mut blob = BatchItem::new("blob", ItemKind::Model, "a round blob creature");
    blob.model_method = Some(ModelMethod::Better);
    let auto = BatchItem::new("bug", ItemKind::Model, "a six-legged armoured insect");
    let r = run_batch(&s, &[blob, auto], "", &d.path().join("out"), &d.path().join("voices"), &CancelToken::default(), &|_, _, _| {});
    assert!(!r[0].ok && r[0].error.as_deref().unwrap_or_default().contains("TRELLIS.2"), "{:?}", r[0]);
    assert!(r[1].ok && r[1].stats["method"] == "quick", "without the chain a model is made the quick way: {:?}", r[1]);
    assert_eq!(seen.lock().unwrap().queued.len(), 2, "nothing was queued for the refused one");
}

/// The real finishing script on a small textured model, when Blender is here.
#[test]
fn blender_finishes_a_pbr_model_with_levels_of_detail_and_engine_textures() {
    let Some(blender) = find_blender(None) else { return };
    let d = tempfile::tempdir().unwrap();
    let make = d.path().join("make.py");
    std::fs::write(
        &make,
        r#"import bpy, sys, os
out = sys.argv[sys.argv.index("--") + 1]
for o in list(bpy.data.objects):
    bpy.data.objects.remove(o, do_unlink=True)
bpy.ops.mesh.primitive_uv_sphere_add(segments=64, ring_count=32, radius=0.5)
def img(name, rgb):
    i = bpy.data.images.new(name, 64, 64)
    i.pixels.foreach_set(list(rgb + (1.0,)) * 64 * 64)
    i.filepath_raw = os.path.join(out, name + ".png"); i.file_format = "PNG"; i.save()
img("basecolor", (0.8, 0.2, 0.1)); img("metallic", (0.9, 0.9, 0.9)); img("roughness", (0.3, 0.3, 0.3))
img("normal", (0.5, 0.25, 1.0)); img("ao", (0.6, 0.6, 0.6))
bpy.ops.export_scene.gltf(filepath=os.path.join(out, "m.glb"), use_selection=False)
"#,
    )
    .unwrap();
    let made = Process::new(&blender).args(["-b", "--python"]).arg(&make).arg("--").arg(d.path()).timeout(Duration::from_secs(300)).run_checked().unwrap();
    assert!(d.path().join("m.glb").is_file(), "{}", made.stdout_text());
    let s = setup("http://127.0.0.1:9", d.path());
    let maps: Vec<(&str, PathBuf)> = ["basecolor", "metallic", "roughness", "normal", "ao"].iter().map(|m| (*m, d.path().join(format!("{m}.png")))).collect();
    let base = d.path().join("out/ball");
    let report = finish_pbr(&s, &d.path().join("m.glb"), &maps, &base, 1000, 2.0, &CancelToken::default()).unwrap();
    assert_eq!(report["maps_out"], json!(["BaseColor", "Normal", "ORM"]), "{report}");
    let faces = report["faces_out"].as_u64().unwrap();
    assert!(faces <= 1050, "{report}");
    let lods: Vec<u64> = report["lod_faces"].as_array().unwrap().iter().filter_map(Value::as_u64).collect();
    assert!(lods.len() == 2 && lods[0] < faces && lods[1] < lods[0], "{report}");
    assert!((report["size_m"][2].as_f64().unwrap() - 2.0).abs() < 0.01, "scaled to its height: {report}");
    for ext in [".glb", ".fbx", "_preview.png", "_BaseColor.png", "_Normal.png", "_ORM.png"] {
        assert!(PathBuf::from(format!("{}{ext}", base.display())).is_file(), "{ext}");
    }
    // The packed and flipped pictures hold the right numbers.
    let check = d.path().join("check.py");
    std::fs::write(
        &check,
        r#"import bpy, sys
base = sys.argv[sys.argv.index("--") + 1]
def px(name):
    i = bpy.data.images.load(base + name); i.colorspace_settings.name = "Non-Color"
    return [round(v, 2) for v in i.pixels[:3]]
print("ORM", px("_ORM.png")); print("NORMAL", px("_Normal.png"))
"#,
    )
    .unwrap();
    let out = Process::new(&blender).args(["-b", "--python"]).arg(&check).arg("--").arg(&base).timeout(Duration::from_secs(300)).run_checked().unwrap();
    let text = out.stdout_text();
    assert!(text.contains("ORM [0.6, 0.3, 0.9]"), "R ambient occlusion, G roughness, B metallic: {text}");
    assert!(text.contains("NORMAL [0.5, 0.75, 1.0]"), "DirectX: green flipped: {text}");
}
