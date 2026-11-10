//! A stand-in OpenAI-compatible server (no real model): it lists one model and answers
//! descriptions, sorting and pictures as the test says.

use super::*;
use crate::{BackendConfig, BackendOptions};
use craft_anything_engine::inventory::{Inventory, Source, Status};
use craft_anything_engine::workspace::TargetRef;
use craft_anything_engine::{CallOptions, Caller, Engine};
use craft_anything_platform::sync::Mutex;
use std::io::{BufRead, BufReader, Read as _, Write as _};
use std::net::TcpListener;
use std::sync::Arc;

/// What the stand-in saw: each chat request's body.
pub(crate) type Seen = Arc<Mutex<Vec<Value>>>;

/// Start a stand-in at `http://127.0.0.1:<port>/v1`; `answer` makes each chat answer's text.
pub(crate) fn fake_server(answer: fn(&Value) -> String) -> (String, Seen) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/v1", l.local_addr().unwrap());
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    std::thread::spawn(move || {
        for stream in l.incoming() {
            let Ok(mut stream) = stream else { continue };
            let s = s.clone();
            std::thread::spawn(move || {
                let mut r = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                if r.read_line(&mut first).is_err() {
                    return;
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
                let out = if path.ends_with("/models") {
                    json!({"object": "list", "data": [{"id": "local-vision"}]})
                } else {
                    let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                    let text = answer(&req);
                    s.lock().unwrap().push(req);
                    json!({"choices": [{"message": {"role": "assistant", "content": text}, "finish_reason": "stop"}]})
                };
                let out = out.to_string();
                let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}", out.len());
            });
        }
    });
    (base, seen)
}

pub(crate) fn has_picture(req: &Value) -> bool {
    req["messages"].as_array().into_iter().flatten().any(|m| m["content"].as_array().is_some_and(|parts| parts.iter().any(|p| p["type"] == "image_url")))
}

fn user_text(req: &Value) -> String {
    req["messages"][1]["content"][0]["text"].as_str().unwrap_or_default().to_string()
}

fn answers(req: &Value) -> String {
    let system = req["messages"][0]["content"].as_str().unwrap_or_default();
    let user = user_text(req);
    if system == CHECK_SYSTEM {
        // Move the first file to check into the first thing that isn't its own.
        let mut keys = Vec::new();
        let mut file = None;
        let mut in_files = false;
        for line in user.lines() {
            if line.starts_with("Files to check") {
                in_files = true;
                continue;
            }
            let Some(rest) = line.strip_prefix("- ") else { continue };
            let first = rest.split_whitespace().next().unwrap_or_default().to_string();
            if in_files {
                if file.is_none() {
                    let now = rest.rsplit("now in ").next().unwrap_or_default().to_string();
                    file = Some((first, now));
                }
            } else {
                keys.push(first);
            }
        }
        let (f, now) = file.unwrap_or_default();
        let to = keys.iter().find(|k| **k != now).cloned().unwrap_or_default();
        return format!("{{\"moves\": [{{\"file\": \"{f}\", \"thing\": \"{to}\"}}, {{\"file\": \"made-up\", \"thing\": \"{to}\"}}], \"merges\": []}}");
    }
    if system == SORT_SYSTEM {
        return "Here: {\"rules\": [{\"term\": \"glyphid\", \"category\": \"enemies\"}, {\"term\": \"made-up\", \"category\": \"hud\"}, {\"term\": \"crate\", \"category\": \"nonsense\"}]}".into();
    }
    if user.contains("FAIL") {
        return String::new();
    }
    if has_picture(req) { format!("<think>hmm</think> A picture of {}.", user.lines().next().unwrap_or_default()) } else { "From its name only.".into() }
}

/// A project with a few items, a thumbnail for one, and a session whose Assets AI is the stand-in.
fn setup(base: &str) -> (tempfile::TempDir, Workspace, Arc<Engine>) {
    let d = tempfile::tempdir().unwrap();
    let ws = Workspace::create(d.path(), "Game", "A game", TargetRef::None).unwrap();
    let mut inv = Inventory { version: inventory::INVENTORY_VERSION, ..Default::default() };
    let mut add = |name: &str, kind: Kind, thumb: bool| {
        let mut i = Item::new(Source::Build, name, kind, name, Status::Placeholder);
        i.id = name.to_ascii_lowercase();
        if thumb {
            std::fs::create_dir_all(ws.root.join("reference/thumbs")).unwrap();
            std::fs::write(ws.root.join(format!("reference/thumbs/{}.png", i.id)), b"\x89PNG fake").unwrap();
            i.thumb = Some(format!("reference/thumbs/{}.png", i.id));
        }
        inv.items.push(i);
    };
    add("Glyphid_Grunt_MAT_Base", Kind::Texture, true);
    add("Armature_Glyphid_Run_01", Kind::Texture, true);
    add("sfx_bugs_small_die", Kind::Sound, false);
    add("FAIL_this", Kind::Texture, false);
    add("Glyphid_Wing", Kind::Model, false);
    add("Glyphid_Leg", Kind::Model, false);
    inventory::save(&ws.root, &inv).unwrap();
    let mut session = craft_anything_engine::testing::session();
    let mut cfg = AiConfig::default();
    cfg.upsert(BackendEntry {
        config: BackendConfig {
            name: "Local vision".into(),
            kind: BackendKind::OpenaiCompatible,
            model: "local-vision".into(),
            base_url: Some(base.into()),
            ssh_host: None,
            enabled: true,
        },
        options: BackendOptions::default(),
    });
    session.settings.ai = cfg.to_table().unwrap();
    (d, ws, Engine::shared(session, specs()))
}

fn call(e: &Engine, tool: &str, args: Value) -> Result<Value, ToolError> {
    e.call(tool, args, CallOptions::new(Caller::Mcp)).map(|o| o.to_json()["result"].clone())
}

#[test]
fn roles_come_from_names_not_from_the_model() {
    assert_eq!(role_from_name("Armature_Glyphid_Run_01", Kind::Texture).map(|r| r.starts_with("vertex animation")), Some(true));
    assert_eq!(role_from_name("Shellback_Normal", Kind::Texture), Some("normal map"));
    assert_eq!(role_from_name("rock_nrm", Kind::Texture), Some("normal map"));
    assert_eq!(role_from_name("Brood_Masks", Kind::Texture).map(|r| r.starts_with("mask")), Some(true));
    assert_eq!(role_from_name("slime_alpha", Kind::Texture), Some("alpha mask"));
    assert_eq!(role_from_name("Grunt_MAT_Base", Kind::Texture), Some("colour texture"));
    assert_eq!(role_from_name("Swarm", Kind::Texture), None);
    assert_eq!(role_from_name("door", Kind::Sound), Some("sound"));
}

#[test]
fn items_are_described_from_their_pictures_and_kept_once() {
    let (base, seen) = fake_server(answers);
    let (_d, ws, e) = setup(&base);
    let r = call(&e, "asset_describe", json!({"project": ws.root})).unwrap();
    assert_eq!((r["described"].as_u64(), r["failed"].as_u64()), (Some(5), Some(1)), "{r}");
    let inv = inventory::load(&ws.root).unwrap();
    let get = |id: &str| inv.items.iter().find(|i| i.id == id).unwrap().clone();
    let base_tex = get("glyphid_grunt_mat_base");
    assert_eq!(base_tex.description.as_deref(), Some("A picture of Name: Glyphid_Grunt_MAT_Base."), "thinking is dropped");
    assert_eq!(base_tex.described_by.as_deref(), Some("Local vision"));
    assert_eq!(base_tex.role, Some(craft_anything_engine::asset_things::Role::Colour));
    assert_eq!(get("armature_glyphid_run_01").role, Some(craft_anything_engine::asset_things::Role::AnimationData));
    assert_eq!(get("sfx_bugs_small_die").description.as_deref(), Some("From its name only."), "sounds are described without pictures");
    assert!(get("fail_this").description.is_none());
    // The role was given to the model, never asked for.
    let asked = seen.lock().unwrap().clone();
    let armature = asked.iter().find(|q| user_text(q).contains("Armature_Glyphid")).unwrap();
    assert!(user_text(armature).contains("Technical role (from its name): vertex animation data"));
    assert!(has_picture(armature));
    // Again: only the one that failed is asked about.
    let before = asked.len();
    let r = call(&e, "asset_describe", json!({"project": ws.root})).unwrap();
    assert_eq!((r["described"].as_u64(), r["skipped_already_described"].as_u64()), (Some(0), Some(5)), "{r}");
    assert_eq!(seen.lock().unwrap().len(), before + 1);
}

#[test]
fn sorting_applies_only_terms_that_were_asked_about() {
    let (base, _) = fake_server(answers);
    let (_d, ws, e) = setup(&base);
    let r = call(&e, "project_inventory_sort_with_agent", json!({"project": ws.root})).unwrap();
    assert_eq!(r["added"], json!([{"term": "glyphid", "category": "enemies"}]), "{r}");
    let inv = inventory::load(&ws.root).unwrap();
    assert_eq!(inv.items.iter().find(|i| i.id == "glyphid_wing").unwrap().category, Category::Enemies);
    assert!(parse_rules("no json here", &["a".into()]).is_empty());
}

#[test]
fn a_local_server_is_seen_running_and_an_agent_app_cant_be_asked() {
    let (base, _) = fake_server(answers);
    let (_d, _ws, e) = setup(&base);
    let st = call(&e, "ai_backend_status", json!({})).unwrap();
    assert_eq!(st["servers"][0]["state"], "running", "{st}");
    assert_eq!(st["assets_backend"], "Local vision");
    let r = call(&e, "ai_backend_start", json!({"name": "Local vision"}));
    assert!(r.is_err() || r.unwrap()["already_running"] == true);
    let agent = BackendEntry {
        config: BackendConfig {
            name: "Claude Code".into(),
            kind: BackendKind::AgentCli,
            model: "claude-code".into(),
            base_url: None,
            ssh_host: None,
            enabled: true,
        },
        options: BackendOptions::default(),
    };
    assert_eq!(AssetModel::of(&agent).unwrap_err().code, ErrorCode::BadSettings);
    // Nothing listens: not running, and it can't start without a start command.
    let mut gone = agent.clone();
    gone.config = BackendConfig { kind: BackendKind::OpenaiCompatible, base_url: Some("http://127.0.0.1:9/v1".into()), ..gone.config };
    assert_eq!(server_state(&gone)["state"], "not_running");
}

#[test]
fn the_ai_checks_the_grouping_and_its_moves_are_an_agents() {
    let (base, _) = fake_server(answers);
    let (_d, ws, e) = setup(&base);
    let inv = inventory::load(&ws.root).unwrap();
    let category = inv.items.iter().find(|i| i.id == "glyphid_wing").unwrap().category;
    let r = call(&e, "project_inventory_check_grouping", json!({"project": ws.root, "category": category})).unwrap();
    assert_eq!(r["moved"], 1, "only the file that was asked about: {r}");
    let inv = inventory::load(&ws.root).unwrap();
    let moved = inv.items.iter().find(|i| r["moves"][0]["file"].as_str() == Some(i.id.as_str())).unwrap();
    assert_eq!(moved.thing_by, craft_anything_engine::asset_categories::CategoryBy::Agent);
    assert_eq!(moved.thing.as_ref().map(|t| t.key.as_str()), r["moves"][0]["thing"].as_str());
    assert_eq!(parse_grouping("nothing", &[], &[]), (vec![], vec![]));
}

/// Every job the person gives is Claude's, but the asset work stays with the local model: with
/// Claude Code and the Claude API set up too, describing, sorting and checking the grouping all
/// ask the local model.
#[test]
fn the_asset_work_stays_with_the_local_model_when_claude_is_set_up() {
    let (base, seen) = fake_server(answers);
    let (_d, ws, e) = setup(&base);
    {
        let mut s = e.session();
        let mut cfg = AiConfig::from_table(&s.settings.ai).unwrap();
        for (name, kind, model) in [("Claude Code", BackendKind::AgentCli, "claude-code"), ("Claude", BackendKind::Anthropic, "claude-opus-5-5")] {
            cfg.upsert(BackendEntry {
                config: BackendConfig { name: name.into(), kind, model: model.into(), base_url: None, ssh_host: None, enabled: true },
                options: BackendOptions::default(),
            });
        }
        assert_eq!(cfg.backend_for(Role::Builder).unwrap().config.name, "Claude Code");
        assert_eq!(cfg.backend_for(Role::Lead).unwrap().config.name, "Claude");
        assert_eq!(cfg.backend_for(Role::Assets).unwrap().config.name, "Local vision");
        s.settings.ai = cfg.to_table().unwrap();
    }
    let r = call(&e, "asset_describe", json!({"project": ws.root})).unwrap();
    assert_eq!(r["described"].as_u64(), Some(5), "{r}");
    let after_describe = seen.lock().unwrap().len();
    assert!(after_describe >= 5, "the local model described them");
    call(&e, "project_inventory_sort_with_agent", json!({"project": ws.root})).unwrap();
    let after_sort = seen.lock().unwrap().len();
    assert!(after_sort > after_describe, "the local model sorted them");
    let inv = inventory::load(&ws.root).unwrap();
    assert!(inv.items.iter().filter_map(|i| i.described_by.as_deref()).all(|b| b == "Local vision"));
    let category = inv.items.iter().find(|i| i.id == "glyphid_wing").unwrap().category;
    call(&e, "project_inventory_check_grouping", json!({"project": ws.root, "category": category})).unwrap();
    assert!(seen.lock().unwrap().len() > after_sort, "the local model checked the grouping");
}
