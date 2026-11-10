//! A stand-in Blender (a script that does what the helpers would) for the runner, and the real
//! one, when it's installed, for the scripts themselves.

use super::*;

/// A stand-in Blender: reads the job like guard.py, then for `render` writes each output (not for
/// an id with "bad") and prints PREVIEW lines; for `inspect` prints a RESULT; a script named
/// `boom.py` fails.
fn fake_blender(dir: &Path) -> Option<PathBuf> {
    if !cfg!(windows) {
        return None;
    }
    let ps1 = dir.join("fake_blender.ps1");
    std::fs::write(
        &ps1,
        r#"$job = Get-Content -Raw -Encoding utf8 $args[0] | ConvertFrom-Json
if ($job.script -like '*boom.py') { [Console]::Error.WriteLine('Traceback: boom'); exit 1 }
$mode = $job.argv[0]
if ($mode -eq 'inspect') { Write-Output 'Blender 5.2'; Write-Output 'RESULT {"faces": 12, "bounds": {"size": [2, 2, 2]}}'; exit 0 }
if ($mode -eq 'render') {
  $r = Get-Content -Raw -Encoding utf8 $job.argv[1] | ConvertFrom-Json
  $made = 0
  foreach ($i in $r.items) {
    if ($i.id -like '*bad*') { Write-Output ('PREVIEW {"id": "' + $i.id + '", "ok": false, "error": "it has no mesh"}'); continue }
    New-Item -ItemType Directory -Force -Path (Split-Path $i.output) | Out-Null
    Set-Content -Path $i.output -Value 'png'
    Write-Output ('PREVIEW {"id": "' + $i.id + '", "ok": true, "seconds": 0.5}')
    $made++
  }
  Write-Output ('RESULT {"made": ' + $made + '}')
  exit 0
}
Write-Output 'RESULT {"argv": "other"}'
"#,
    )
    .ok()?;
    let cmd = dir.join("blender.cmd");
    // Arguments: -b --python-exit-code 1 --python <guard> -- <job>
    std::fs::write(&cmd, "@powershell -NoProfile -ExecutionPolicy Bypass -File \"%~dp0fake_blender.ps1\" %7\r\n").ok()?;
    Some(cmd)
}

fn setup(dir: &Path, blender: Option<PathBuf>) -> Setup {
    Setup { blender, scripts_dir: dir.join("scripts") }
}

#[test]
fn results_and_tails_are_read() {
    assert_eq!(result_line("noise\nRESULT {\"a\": 1}\nRESULT {\"a\": 2}\nmore"), Some(json!({"a": 2})));
    assert_eq!(result_line("RESULT not json"), None);
    assert_eq!(tail("abcdef", 3), "…def");
    assert!(is_model_file(Path::new("a/B.GLB")) && !is_model_file(Path::new("a.png")));
}

#[test]
fn the_runner_reads_results_failures_and_previews() {
    let d = tempfile::tempdir().unwrap();
    let Some(fake) = fake_blender(d.path()) else { return };
    let s = setup(d.path(), Some(fake));
    let c = CancelToken::default();
    let stats = inspect(&s, &d.path().join("m.glb"), &c).unwrap();
    assert_eq!(stats["faces"], 12);
    let boom = d.path().join("boom.py");
    std::fs::write(&boom, "raise SystemExit(1)").unwrap();
    let r = run_script(&s, &boom, &[], &[d.path().to_path_buf()], DEFAULT_TIMEOUT, &c, None, &|| {}).unwrap();
    assert!(!r.ok && r.exit_code == Some(1) && r.result.is_none());
    assert!(r.stderr_tail.contains("boom"), "{r:?}");
    let out = d.path().join("proj/reference/thumbs");
    let items: Vec<Preview> = ["a", "bad-one", "c"]
        .iter()
        .map(|id| Preview { id: id.to_string(), input: d.path().join(format!("{id}.obj")), output: out.join(format!("{id}.png")) })
        .collect();
    let seen = craft_anything_platform::sync::Mutex::new(Vec::new());
    let r = render_previews(&s, &items, &d.path().join("proj"), &c, &|n, t| seen.lock().unwrap().push((n, t))).unwrap();
    assert_eq!(r.iter().map(|r| r.ok).collect::<Vec<_>>(), [true, false, true], "{r:?}");
    assert_eq!(r[1].error.as_deref(), Some("it has no mesh"));
    assert!(out.join("a.png").is_file());
    assert_eq!(seen.lock().unwrap().last(), Some(&(3, 3)), "it said how far it got");
    // Nothing left behind but the helper scripts.
    let left: Vec<String> = std::fs::read_dir(d.path().join("scripts")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    assert!(left.iter().all(|f| f.ends_with(".py")), "{left:?}");
}

#[test]
fn without_blender_it_says_so() {
    let d = tempfile::tempdir().unwrap();
    let s = setup(d.path(), Some(d.path().join("no-blender.exe")));
    if find_blender(None).is_some() {
        return; // found on this computer anyway
    }
    assert_eq!(inspect(&s, &d.path().join("m.glb"), &CancelToken::default()).unwrap_err().code, ErrorCode::EngineMissing);
}

/// The real Blender, when it's installed: a cube is read and rendered, and a script can't write
/// outside its folder, start a program or reset Blender to factory settings.
#[test]
fn real_blender_reads_renders_and_keeps_writes_in_the_project() {
    let Some(blender) = find_blender(None) else { return };
    let d = tempfile::tempdir().unwrap();
    let s = setup(d.path(), Some(blender));
    let proj = d.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    let cube = proj.join("cube.obj");
    std::fs::write(&cube, "v -1 -1 -1\nv 1 -1 -1\nv 1 1 -1\nv -1 1 -1\nv -1 -1 1\nv 1 -1 1\nv 1 1 1\nv -1 1 1\nf 1 2 3 4\nf 5 8 7 6\nf 1 5 6 2\nf 2 6 7 3\nf 3 7 8 4\nf 5 1 4 8\n").unwrap();
    let c = CancelToken::default();
    let stats = inspect(&s, &cube, &c).unwrap();
    assert_eq!((stats["faces"].as_u64(), stats["triangles"].as_u64()), (Some(6), Some(12)), "{stats}");
    assert_eq!(stats["bounds"]["size"], json!([2.0, 2.0, 2.0]));
    let out = proj.join("reference/thumbs/cube.png");
    let r = render_previews(&s, &[Preview { id: "cube".into(), input: cube.clone(), output: out.clone() }], &proj, &c, &|_, _| {}).unwrap();
    assert!(r[0].ok, "{r:?}");
    assert!(std::fs::read(&out).unwrap().starts_with(b"\x89PNG"));
    let evil = proj.join("evil.py");
    std::fs::write(
        &evil,
        "import bpy, os, json\nres = {}\n\
         for what, f in [('open', lambda: open(os.path.join(os.path.dirname(os.path.dirname(__file__)), 'outside.txt'), 'w')),\n\
                         ('inside', lambda: open(os.path.join(os.path.dirname(__file__), 'inside.txt'), 'w').close()),\n\
                         ('factory', lambda: bpy.ops.wm.read_factory_settings()),\n\
                         ('process', lambda: os.system('echo hi'))]:\n\
         \x20   try:\n\
         \x20       f(); res[what] = 'allowed'\n\
         \x20   except PermissionError:\n\
         \x20       res[what] = 'refused'\n\
         print('RESULT ' + json.dumps(res))\n",
    )
    .unwrap();
    let r = run_script(&s, &evil, &[], std::slice::from_ref(&proj), DEFAULT_TIMEOUT, &c, None, &|| {}).unwrap();
    assert_eq!(r.result, Some(json!({"open": "refused", "inside": "allowed", "factory": "refused", "process": "refused"})), "{r:?}");
    assert!(!d.path().join("outside.txt").exists() && proj.join("inside.txt").exists());
}

/// The real Blender: an agent's script uses the helpers, writes relative files into its folder,
/// and an absolute path outside the project is refused with a clear message.
#[test]
fn real_blender_runs_an_agent_script_with_its_helpers() {
    let Some(blender) = find_blender(None) else { return };
    let d = tempfile::tempdir().unwrap();
    let s = setup(d.path(), Some(blender));
    let proj = d.path().join("proj");
    let out = proj.join("blender/run-1");
    let outside = d.path().join("outside.glb").display().to_string().replace('\\', "/");
    let code = format!(
        "clear_scene()\nbpy.ops.mesh.primitive_cube_add(size=1)\ncube = bpy.context.object\ncube.data.materials.append(new_material('Crate', (0.6, 0.4, 0.2, 1)))\n\
         glb = export_glb('crate.glb')\npng = render_preview('preview.png', size=128)\n\
         try:\n    bpy.ops.export_scene.gltf(filepath='{outside}')\n    refused = False\nexcept PermissionError as e:\n    refused = 'outside the project' in str(e)\n\
         result(glb=glb, png=png, refused=refused)\n"
    );
    let r = run_agent_script(&s, &code, &proj, &out, DEFAULT_TIMEOUT, &CancelToken::default()).unwrap();
    assert!(r.run.ok, "{r:?}");
    let res = r.run.result.clone().unwrap();
    assert_eq!(res["refused"], true, "{res}");
    assert!(out.join("crate.glb").is_file() && out.join("preview.png").is_file());
    assert!(r.new_files.iter().any(|f| f.ends_with("preview.png")), "{:?}", r.new_files);
    assert!(!Path::new(&outside).exists());
}
