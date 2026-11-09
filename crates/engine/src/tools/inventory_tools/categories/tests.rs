use crate::inventory::{self, INVENTORY_VERSION, Inventory, Item, Kind, Source, Status};
use crate::workspace::{TargetRef, Workspace};
use crate::{CallOptions, Caller, Engine};
use craft_anything_evidence::ErrorCode;
use serde_json::{Value, json};

fn engine(deep: bool) -> Engine {
    let mut s = crate::testing::session();
    s.settings.deep_mode = deep;
    Engine::new(s)
}

fn call(e: &Engine, id: &str, params: Value) -> Value {
    e.call(id, params, CallOptions::new(Caller::Mcp)).unwrap_or_else(|err| panic!("{id}: {err:?}")).to_json()["result"].clone()
}

/// A project whose inventory has a few named items, as a scan of an original would leave them.
fn project() -> (tempfile::TempDir, Workspace) {
    let d = tempfile::tempdir().unwrap();
    let ws = Workspace::create_at(&d.path().join("proj"), "Cats", "make it", TargetRef::None, None).unwrap();
    let names = [
        ("Icons_CM_Back", Kind::Texture),
        ("HexButton_Hover", Kind::Texture),
        ("Glyphid_Grunt", Kind::Texture),
        ("Glyphid_Praetorian", Kind::Model),
        ("sfx_DRAK_25_shot", Kind::Sound),
        ("Scout_BEARD", Kind::Model),
        ("Tumbler_Body", Kind::Model),
        ("Main_Theme", Kind::Music),
    ];
    let items: Vec<Item> = names
        .iter()
        .map(|(n, k)| {
            let mut i = Item::new(Source::Original, n, *k, *n, Status::Original);
            i.path = Some(format!("reference/assets/{n}"));
            i
        })
        .collect();
    let mut inv = Inventory { version: INVENTORY_VERSION, items, ..Default::default() };
    crate::asset_categories::apply(&mut inv);
    inventory::save(&ws.root, &inv).unwrap();
    (d, ws)
}

fn categories(e: &Engine, ws: &Workspace) -> Value {
    call(e, "project_inventory_list", json!({"project": ws.root, "limit": 100}))["counts"]["by_category"].clone()
}

#[test]
fn rules_terms_and_agents_sort_a_projects_assets() {
    let (_d, ws) = project();
    let e = engine(true);
    // The built-in keywords: icons and buttons are the HUD, music is music, "body" is a character part.
    assert_eq!(categories(&e, &ws), json!({"characters": 2, "hud": 2, "music": 1, "unsorted": 2, "weapons": 1}));
    // What's left unsorted, by word.
    let terms = call(&e, "project_inventory_unsorted_terms", json!({"project": ws.root}));
    assert_eq!(terms["unsorted"], 2);
    assert_eq!(terms["terms"][0], json!({"term": "glyphid", "items": 2, "examples": ["Glyphid_Grunt", "Glyphid_Praetorian"]}));
    // Project terms sort them, and win over the built-ins ("tumbler body" is a machine, not a body).
    let r = call(
        &e,
        "project_inventory_category_rules",
        json!({"project": ws.root, "add": [{"term": "Glyphid", "category": "enemies"}, {"term": "tumbler", "category": "machines"}]}),
    );
    assert_eq!(r["by_category"], json!({"characters": 1, "enemies": 2, "hud": 2, "machines": 1, "music": 1, "weapons": 1}));
    let listed = call(&e, "project_inventory_category_rules", json!({"project": ws.root}));
    assert_eq!(listed["rules"].as_array().unwrap().len(), 2);
    // An agent sorts by glob; the person's choice stays over an agent's.
    let list = call(&e, "project_inventory_list", json!({"project": ws.root, "query": "HexButton"}));
    let id = list["items"][0]["id"].as_str().unwrap().to_string();
    call(&e, "project_inventory_categorize", json!({"project": ws.root, "ids": [id], "category": "effects", "by": "user"}));
    let r = call(&e, "project_inventory_categorize", json!({"project": ws.root, "name_glob": "*hover", "category": "machines"}));
    assert_eq!((r["changed"].as_u64(), r["kept_as_the_person_chose"].as_u64()), (Some(0), Some(1)));
    let r = call(&e, "project_inventory_categorize", json!({"project": ws.root, "name_glob": "sfx_drak*", "category": "weapons"}));
    assert_eq!(r["names"], json!(["sfx_DRAK_25_shot"]));
    let effects = call(&e, "project_inventory_list", json!({"project": ws.root, "category": "effects"}));
    assert_eq!(effects["items"][0]["category_by"], "user");
    // Removing a rule sends its items back to the built-ins; people's and agents' choices stay.
    let r = call(&e, "project_inventory_category_rules", json!({"project": ws.root, "remove": ["glyphid"]}));
    assert_eq!(r["by_category"]["unsorted"], 2);
    assert_eq!(r["by_category"]["effects"], 1);
    // Bad input is refused plainly.
    let err = e.call("project_inventory_categorize", json!({"project": ws.root, "category": "hud"}), CallOptions::new(Caller::Mcp)).unwrap_err();
    assert_eq!(err.code, ErrorCode::BadInput);
    let err =
        e.call("project_inventory_categorize", json!({"project": ws.root, "ids": ["x"], "category": "spaceships"}), CallOptions::new(Caller::Mcp)).unwrap_err();
    assert_eq!(err.code, ErrorCode::BadInput);
}

#[test]
fn a_rescan_keeps_chosen_categories_and_old_files_are_sorted_when_read() {
    let (_d, ws) = project();
    let e = engine(false);
    // A version-1 file (no categories) is sorted by the rules when it's read.
    let mut doc: Value = serde_json::from_slice(&std::fs::read(ws.root.join(inventory::INVENTORY)).unwrap()).unwrap();
    doc["version"] = json!(1);
    for item in doc["items"].as_array_mut().unwrap() {
        item.as_object_mut().unwrap().remove("category");
        item.as_object_mut().unwrap().remove("category_by");
    }
    std::fs::write(ws.root.join(inventory::INVENTORY), serde_json::to_vec(&doc).unwrap()).unwrap();
    let inv = inventory::load(&ws.root).unwrap();
    assert_eq!(inv.version, INVENTORY_VERSION, "brought up to date as it is read");
    assert!(inv.items.iter().any(|i| i.name == "Icons_CM_Back" && i.category == crate::asset_categories::Category::Hud));
    // A build item sorted by an agent keeps its category through a rescan.
    let app = ws.app_dir();
    std::fs::create_dir_all(app.join("Content")).unwrap();
    std::fs::write(app.join("Content/odd_thing.png"), crate::inventory::tests::png(2, 2)).unwrap();
    call(&e, "project_inventory_scan", json!({"project": ws.root}));
    let r = call(&e, "project_inventory_categorize", json!({"project": ws.root, "name_glob": "odd_thing*", "category": "items"}));
    assert_eq!(r["changed"], 1);
    call(&e, "project_inventory_scan", json!({"project": ws.root}));
    let list = call(&e, "project_inventory_list", json!({"project": ws.root, "source": "build"}));
    assert_eq!((list["items"][0]["category"].as_str(), list["items"][0]["category_by"].as_str()), (Some("items"), Some("agent")));
    let saved = inventory::load(&ws.root).unwrap();
    assert_eq!(saved.version, INVENTORY_VERSION);
}

#[test]
fn a_plan_is_cheapest_first_and_never_starts_what_isnt_set_up() {
    let (_d, ws) = project();
    let e = engine(true);
    let hud = call(&e, "project_inventory_rebuild_plan", json!({"project": ws.root, "category": "hud"}));
    assert_eq!(hud["total"], "Free");
    assert_eq!(hud["rows"][0]["method"], "engine_ui");
    call(&e, "project_inventory_category_rules", json!({"project": ws.root, "add": [{"term": "Glyphid", "category": "enemies"}]}));
    let enemies = call(&e, "project_inventory_rebuild_plan", json!({"project": ws.root, "category": "enemies", "methods": {"model": "tripo"}}));
    let model = enemies["rows"].as_array().unwrap().iter().find(|r| r["kind"] == "model").unwrap().clone();
    assert_eq!((model["available"].as_bool(), model["credits"].as_u64()), (Some(false), Some(40)));
    assert_eq!(enemies["total"], "about 40 Tripo credits");
    let tripo = model["options"].as_array().unwrap().iter().find(|o| o["id"] == "tripo").unwrap().clone();
    assert_eq!(tripo["why"], "Not set up yet — next step");
    let err = e
        .call("project_inventory_rebuild_plan", json!({"project": ws.root, "category": "hud", "methods": {"texture": "tripo"}}), CallOptions::new(Caller::Mcp))
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::BadInput, "Tripo isn't offered for the HUD");
    // Starting (the engine's half): Tripo is skipped, the texture goes to Blender, nothing else changes.
    let mut inv = inventory::load(&ws.root).unwrap();
    let methods = [("model".to_string(), "tripo".to_string())].into_iter().collect();
    let plan = {
        let items = crate::asset_rebuild::select(&inv, Some(crate::asset_categories::Category::Enemies), &[], |_| true).unwrap();
        crate::asset_rebuild::Plan::new(&items, Some(crate::asset_categories::Category::Enemies), &methods).unwrap()
    };
    let done = crate::asset_rebuild::start(&ws, &mut inv, &plan, 1_800_000_000_000).unwrap();
    let started = json!({"started": done.items, "skipped": [{"method": done.skipped[0].1}], "list": done.list.display().to_string(), "brief": done.brief});
    assert_eq!(started["started"], 1);
    assert_eq!(started["skipped"][0]["method"], "tripo");
    let list = std::path::PathBuf::from(started["list"].as_str().unwrap());
    let doc: Value = serde_json::from_slice(&std::fs::read(&list).unwrap()).unwrap();
    assert_eq!(doc["items"][0]["name"], "Glyphid_Grunt");
    assert_eq!(doc["items"][0]["method"], "blender");
    assert!(started["brief"].as_str().unwrap().contains("never use paid services"));
    let inv = inventory::load(&ws.root).unwrap();
    let grunt = inv.items.iter().find(|i| i.name == "Glyphid_Grunt").unwrap();
    assert_eq!(grunt.status, Status::NeedsReplacing);
    assert_eq!(grunt.rebuild.as_ref().map(|r| r.method.as_str()), Some("blender"));
    let praetorian = inv.items.iter().find(|i| i.name == "Glyphid_Praetorian").unwrap();
    assert_eq!(praetorian.status, Status::Original, "skipped: left alone");
}
