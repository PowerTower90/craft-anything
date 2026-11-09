use super::original::{parse_clip, safe_name, sound_format};
use crate::inventory::tests::{glb, png, wav};
use crate::inventory::{self, INVENTORY, Kind, Source, Status, picture_size};
use crate::workspace::{AssetRecord, TargetRef, Workspace};
use crate::{CallOptions, Caller, Engine};
use craft_anything_evidence::ErrorCode;
use craft_anything_testkit::games::{unity_serialized_file, unity_texture2d, unreal_pak};
use serde_json::{Value, json};
use std::path::Path;

fn engine(deep: bool) -> Engine {
    let mut s = crate::testing::session();
    s.settings.deep_mode = deep;
    Engine::new(s)
}

fn call(e: &Engine, id: &str, params: Value) -> Value {
    e.call(id, params, CallOptions::new(Caller::Mcp)).unwrap_or_else(|err| panic!("{id}: {err:?}")).to_json()["result"].clone()
}

fn put(dir: &Path, rel: &str, bytes: &[u8]) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, bytes).unwrap();
}

fn project(target: TargetRef) -> (tempfile::TempDir, Workspace) {
    let d = tempfile::tempdir().unwrap();
    let ws = Workspace::create_at(&d.path().join("proj"), "Inv", "make it", target, None).unwrap();
    (d, ws)
}

fn ids_of(list: &Value) -> Vec<String> {
    list["items"].as_array().unwrap().iter().map(|i| i["id"].as_str().unwrap().to_string()).collect()
}

#[test]
fn a_build_is_scanned_listed_and_marked() {
    let (_d, ws) = project(TargetRef::None);
    let app = ws.app_dir();
    put(&app, "Content/Textures/hero.png", &png(300, 100));
    put(&app, "Content/Sfx/jump.wav", &wav(1));
    put(&app, "Content/Models/crate.glb", &glb());
    put(&app, "Content/Props/SM_Crate.uasset", b"package");
    put(&app, "Content/gen/sky.png", &png(2, 2));
    put(&app, "Intermediate/cache.png", &png(2, 2));
    put(&app, "Source/Game.cpp", b"int main() {}");
    ws.record_asset(AssetRecord { file: "Content/gen/sky.png".into(), kind: "image".into(), prompt: "a sky".into(), cost_usd: 0.0, at_ms: 0 }).unwrap();
    // Clean room: the build is scanned, the original isn't.
    let e = engine(false);
    let v = call(&e, "project_inventory_scan", json!({"project": ws.root}));
    assert_eq!(v["counts"]["by_source"], json!({"build": 5}), "{v}");
    assert_eq!(v["counts"]["by_kind"]["build"], json!({"model": 2, "sound": 1, "texture": 2}));
    assert_eq!(v["counts"]["by_status"], json!({"generated": 1, "placeholder": 4}));
    assert_eq!(v["original"]["scanned"], false);
    assert_eq!(v["thumbnails"]["made"], 2);
    assert!(ws.root.join(INVENTORY).is_file());

    let list = call(&e, "project_inventory_list", json!({"project": ws.root, "kind": "texture", "query": "HERO"}));
    assert_eq!(list["matching"], 1);
    let hero = &list["items"][0];
    assert_eq!((hero["width"].as_u64(), hero["height"].as_u64(), hero["status"].as_str()), (Some(300), Some(100), Some("placeholder")));
    assert_eq!(hero["path"], "app/Content/Textures/hero.png");
    let thumb = ws.root.join(hero["thumb"].as_str().unwrap());
    assert_eq!(picture_size(&thumb), Some((256, 85)));
    let sound = call(&e, "project_inventory_list", json!({"project": ws.root, "kind": "sound"}));
    assert_eq!(sound["items"][0]["duration_secs"].as_f64(), Some(1.0));
    let paged = call(&e, "project_inventory_list", json!({"project": ws.root, "offset": 1, "limit": 2}));
    assert_eq!((paged["matching"].as_u64(), paged["items"].as_array().unwrap().len()), (Some(5), 2));

    // Marking, then a rescan that keeps the marks.
    let hero_id = hero["id"].as_str().unwrap().to_string();
    let marked = call(&e, "project_inventory_mark", json!({"project": ws.root, "ids": [hero_id, "nope"], "status": "needs_replacing", "note": "off-model"}));
    assert_eq!((marked["updated"][0]["status"].as_str(), marked["not_found"][0].as_str()), (Some("needs_replacing"), Some("nope")));
    put(&app, "Content/Textures/hero.png", &png(64, 64));
    put(&app, "Content/Textures/villain.png", &png(8, 8));
    call(&e, "project_inventory_scan", json!({"project": ws.root}));
    let list = call(&e, "project_inventory_list", json!({"project": ws.root, "status": "needs_replacing"}));
    assert_eq!(ids_of(&list), std::slice::from_ref(&hero_id));
    assert_eq!((list["items"][0]["note"].as_str(), list["items"][0]["width"].as_u64()), (Some("off-model"), Some(64)), "the mark stays; the facts are new");
    let by_query = call(&e, "project_inventory_list", json!({"project": ws.root, "query": "off-model"}));
    assert_eq!(ids_of(&by_query), [hero_id], "the query looks at notes too");

    // Bad input is refused plainly.
    let err = e.call("project_inventory_mark", json!({"project": ws.root, "ids": [], "status": "replaced"}), CallOptions::new(Caller::Mcp)).unwrap_err();
    assert_eq!(err.code, ErrorCode::BadInput);
    let err = e.call("project_inventory_list", json!({"project": ws.root, "kind": "spaceship"}), CallOptions::new(Caller::Mcp)).unwrap_err();
    assert_eq!(err.code, ErrorCode::BadInput);
    let err = e.call("project_inventory_list", json!({"project": ws.root.join("nowhere")}), CallOptions::new(Caller::Mcp)).unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
}

#[test]
fn clean_room_never_reads_the_original_or_shows_it() {
    let (_d, ws) = project(TargetRef::None);
    let e = engine(false);
    let err = e.call("project_inventory_scan", json!({"project": ws.root, "original": true}), CallOptions::new(Caller::Mcp)).unwrap_err();
    assert!(crate::clean_room::is_refusal(&err), "{err:?}");
    assert!(!ws.root.join(INVENTORY).exists(), "nothing is written");
    // An inventory a Deep scan made: Clean room lists and marks only the build's items.
    let mut inv = inventory::load(&ws.root).unwrap();
    let mut orig = inventory::Item::new(Source::Original, "game#1", Kind::Texture, "Wall", Status::Original);
    orig.origin = Some("game#1".into());
    let orig_id = orig.id.clone();
    inv.items.push(orig);
    inv.items.push(inventory::Item::new(Source::Build, "app/a.png", Kind::Texture, "a.png", Status::Placeholder));
    inventory::save(&ws.root, &inv).unwrap();
    let list = call(&e, "project_inventory_list", json!({"project": ws.root}));
    assert_eq!((list["matching"].as_u64(), list["counts"]["original_hidden_in_clean_room"].as_u64()), (Some(1), Some(1)));
    let marked = call(&e, "project_inventory_mark", json!({"project": ws.root, "ids": [orig_id], "status": "replaced"}));
    assert_eq!(marked["not_found"].as_array().unwrap().len(), 1);
    let deep = call(&engine(true), "project_inventory_list", json!({"project": ws.root, "source": "original"}));
    assert_eq!(deep["matching"], 1);
}

/// A Unity AudioClip object (Unity 2017.1 and later) whose data is `size` bytes at `offset` of `source`.
fn audio_clip(name: &str, secs: f32, source: &str, offset: u64, size: u64) -> Vec<u8> {
    let mut v = Vec::new();
    let string = |v: &mut Vec<u8>, s: &str| {
        v.extend((s.len() as u32).to_le_bytes());
        v.extend(s.as_bytes());
        while !v.len().is_multiple_of(4) {
            v.push(0);
        }
    };
    string(&mut v, name);
    for x in [1u32, 2, 48_000, 16] {
        v.extend(x.to_le_bytes()); // load type, channels, frequency, bits
    }
    v.extend(secs.to_le_bytes());
    v.extend([0, 0, 0, 0]); // tracker, ambisonic, align
    v.extend(0u32.to_le_bytes()); // subsound
    v.extend([1, 0, 0, 0]); // preload, background, legacy 3D, align
    string(&mut v, source);
    v.extend(offset.to_le_bytes());
    v.extend(size.to_le_bytes());
    v.extend(2u32.to_le_bytes()); // compression
    v
}

fn fsb5(mode: u32) -> Vec<u8> {
    let mut v = b"FSB5".to_vec();
    for x in [1u32, 1, 0x24, 0, 0x10, mode] {
        v.extend(x.to_le_bytes());
    }
    v.resize(64, 7);
    v
}

/// A Unity 2022.3 game folder: a texture, a sound kept in a .resource, a mesh, and loose files.
fn unity_game(dir: &Path) {
    let data = dir.join("Game_Data");
    let named = |name: &str| {
        let mut v = (name.len() as u32).to_le_bytes().to_vec();
        v.extend(name.as_bytes());
        v.resize(64, 0);
        v
    };
    let assets = unity_serialized_file(
        "2022.3.62f1",
        &[
            (28, unity_texture2d("2022.3.62f1", "Wall", 2, 1, 4, Some(&[255, 0, 0, 255, 0, 0, 255, 255]), None)),
            (83, audio_clip("Battle Music", 95.5, "sharedassets0.resource", 64, 64)),
            (83, audio_clip("Click", 0.25, "sharedassets0.resource", 0, 64)),
            (43, named("Drill")),
            (28, unity_texture2d("2022.3.62f1", "Fancy", 4, 4, 28, Some(&[0; 16]), None)),
        ],
    );
    put(&data, "sharedassets0.assets", &assets);
    let mut resource = fsb5(15);
    resource.extend(fsb5(2));
    put(&data, "sharedassets0.resource", &resource);
    put(&data, "StreamingAssets/intro.mp4", b"\0\0\0\x18ftypmp42 not really a video");
    put(dir, "Sounds/beep.wav", &wav(1));
}

#[test]
fn the_original_unity_games_assets_are_taken_out_once() {
    let d = tempfile::tempdir().unwrap();
    let game = d.path().join("game");
    unity_game(&game);
    let (_p, ws) = project(TargetRef::Local { path: game.clone() });
    let e = engine(true);
    let v = call(&e, "project_inventory_scan", json!({"project": ws.root, "rebuild": false}));
    let o = &v["original"];
    assert_eq!((o["scanned"].as_bool(), o["items"].as_u64(), o["taken_out"].as_u64()), (Some(true), Some(7), Some(5)), "{v}");
    assert_eq!(
        v["counts"]["by_kind"]["original"],
        json!({"model": 1, "music": 1, "sound": 2, "texture": 2, "video": 1}),
        "the long clip is music; the loose WAV is a sound"
    );
    let list = |q: &str| call(&e, "project_inventory_list", json!({"project": ws.root, "query": q}))["items"][0].clone();
    let wall = list("Wall");
    assert_eq!(wall["path"], "reference/assets/texture/sharedassets0.assets/Wall_1.png");
    assert_eq!(wall["origin"], "Game_Data/sharedassets0.assets#1");
    assert_eq!((wall["width"].as_u64(), wall["status"].as_str()), (Some(2), Some("original")));
    assert_eq!(picture_size(&ws.root.join(wall["path"].as_str().unwrap())), Some((2, 1)));
    assert!(ws.root.join(wall["thumb"].as_str().unwrap()).is_file());
    let music = list("Battle");
    assert_eq!((music["kind"].as_str(), music["duration_secs"].as_f64()), (Some("music"), Some(95.5)));
    assert_eq!(music["path"], "reference/assets/music/sharedassets0.assets/Battle_Music_2.fsb");
    assert!(music["detail"].as_str().unwrap().contains("PCM16"), "{music}");
    let click = std::fs::read(ws.root.join(list("Click")["path"].as_str().unwrap())).unwrap();
    assert_eq!(click, fsb5(15), "the clip's own bytes of the resource");
    let drill = list("Drill");
    assert!(drill["path"].is_null() && drill["detail"].as_str().unwrap().contains("Mesh"), "{drill}");
    let fancy = list("Fancy");
    assert!(fancy["path"].is_null() && fancy["problem"].as_str().unwrap().contains("Crunched"), "a texture that can't be decoded is listed with why: {fancy}");
    let beep = list("beep.wav");
    assert_eq!(beep["path"], "reference/assets/sound/loose/Sounds/beep.wav");
    assert_eq!(beep["duration_secs"].as_f64(), Some(1.0));
    assert!(list("intro")["path"].as_str().unwrap().ends_with("StreamingAssets/intro.mp4"));

    // Marks survive, and a second scan reuses what's there.
    let wall_id = wall["id"].as_str().unwrap();
    call(&e, "project_inventory_mark", json!({"project": ws.root, "ids": [wall_id], "status": "replaced", "note": "done"}));
    let again = call(&e, "project_inventory_scan", json!({"project": ws.root}));
    assert_eq!((again["original"]["taken_out"].as_u64(), again["original"]["already_there"].as_u64()), (Some(0), Some(5)), "{again}");
    let wall = list("Wall");
    assert_eq!((wall["status"].as_str(), wall["note"].as_str(), wall["id"].as_str()), (Some("replaced"), Some("done"), Some(wall_id)));
    // A build-only scan leaves the original's items alone.
    let build_only = call(&e, "project_inventory_scan", json!({"project": ws.root, "original": false}));
    assert_eq!(build_only["counts"]["by_source"]["original"], 7);
}

#[test]
fn an_original_that_cant_be_read_is_said_not_crashed_on() {
    let d = tempfile::tempdir().unwrap();
    let game = d.path().join("game");
    put(&game, "Game_Data/sharedassets0.assets", b"\0\0\0\0garbage that is not a unity file at all, long enough to have a header");
    put(&game, "Game_Data/level0", &[0xff; 10]);
    put(&game, "Art/broken.png", b"\x89PNG\r\n\x1a\nnope");
    let (_p, ws) = project(TargetRef::Local { path: game });
    let e = engine(true);
    let v = call(&e, "project_inventory_scan", json!({"project": ws.root}));
    let notes = v["original"]["notes"].as_array().unwrap();
    assert!(notes.iter().any(|n| n.as_str().unwrap().contains("sharedassets0.assets couldn't be read")), "{v}");
    let broken = call(&e, "project_inventory_list", json!({"project": ws.root, "query": "broken"}));
    assert!(broken["items"][0]["problem"].is_string(), "{broken}");

    // No original at all: the build still saves, with a note.
    let (_q, ws) = project(TargetRef::Website { url: "https://example.com".into() });
    let v = call(&e, "project_inventory_scan", json!({"project": ws.root}));
    assert!(v["original"]["notes"][0].as_str().unwrap().contains("website"), "{v}");
    assert!(ws.root.join(INVENTORY).is_file());
}

#[test]
fn unreal_paks_need_permission_to_be_taken_apart() {
    let d = tempfile::tempdir().unwrap();
    let game = d.path().join("game");
    let pak = unreal_pak(&[
        ("Game/Content/Audio/", "hit.wav", &wav(2), true),
        ("Game/Content/Textures/", "T_Rock.uasset", b"not a package", false),
        ("Game/Content/Textures/", "T_Rock.uexp", b"x", false),
        ("Game/Content/Meshes/", "SM_Rock.uasset", b"mesh", false),
        ("Game/Config/", "DefaultGame.ini", b"[x]", false),
    ]);
    put(&game, "Game/Content/Paks/Game-Windows.pak", &pak);
    let (_p, ws) = project(TargetRef::Local { path: game });
    let e = engine(true);
    let v = call(&e, "project_inventory_scan", json!({"project": ws.root}));
    assert_eq!((v["original"]["items"].as_u64(), v["original"]["taken_out"].as_u64()), (Some(3), Some(0)), "{v}");
    assert!(v["original"]["notes"].as_array().unwrap().iter().any(|n| n.as_str().unwrap().contains("permission: true")));
    let v = call(&e, "project_inventory_scan", json!({"project": ws.root, "permission": true}));
    assert_eq!(v["original"]["taken_out"], 1, "{v}");
    let hit = call(&e, "project_inventory_list", json!({"project": ws.root, "query": "hit.wav"}))["items"][0].clone();
    assert_eq!(hit["path"], "reference/assets/sound/Game-Windows/Game/Content/Audio/hit.wav");
    assert_eq!(hit["duration_secs"].as_f64(), Some(2.0));
    let rock = call(&e, "project_inventory_list", json!({"project": ws.root, "query": "T_Rock"}))["items"][0].clone();
    assert!(rock["path"].is_null() && rock["problem"].is_string(), "{rock}");
    let mesh = call(&e, "project_inventory_list", json!({"project": ws.root, "kind": "model"}))["items"][0].clone();
    assert_eq!(mesh["origin"], "Game/Content/Paks/Game-Windows.pak:Game/Content/Meshes/SM_Rock.uasset");
}

#[test]
fn audio_clips_and_names_are_read_carefully() {
    let clip = parse_clip(&audio_clip("Boom", 1.5, "a.resource", 8, 16), false, (2022, 3)).unwrap();
    assert_eq!((clip.name.as_str(), clip.channels, clip.frequency, clip.source.as_str(), clip.offset, clip.size), ("Boom", 2, 48_000, "a.resource", 8, 16));
    assert_eq!(parse_clip(&audio_clip("Boom", 1.5, "a.resource", 8, 16)[..20], false, (2022, 3)), None, "cut short");
    assert_eq!(parse_clip(&audio_clip("Boom", f32::NAN, "a", 0, 1), false, (2022, 3)), None, "a length that isn't a number");
    assert_eq!(parse_clip(&[0xff; 64], false, (2022, 3)), None);
    assert_eq!(sound_format(b"OggS....").0, "ogg");
    assert!(sound_format(&fsb5(99)).1.contains("codec 99"));
    assert_eq!(sound_format(b"").0, "bin");
    assert_eq!(safe_name("a/b\\c:d*e", 80), "a_b_c_d_e");
    assert_eq!(safe_name("..", 80), "unnamed");
    assert_eq!(safe_name("LPT1.txt", 80), "_LPT1.txt");
    assert_eq!(safe_name("Ünïcode name", 80), "_n_code_name");
    assert_eq!(safe_name(&"x".repeat(300), 80).len(), 80);
}
