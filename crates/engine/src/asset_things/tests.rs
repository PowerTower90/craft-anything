//! The grouper on the name patterns real games use: typos, parts and outfits, animation
//! textures, maps, sounds and voice lines.

use super::*;
use crate::asset_categories::{self, CategoryRule};
use crate::inventory::{INVENTORY_VERSION, Source, Status};

fn item(name: &str, kind: Kind, category: Category) -> Item {
    let mut i = Item::new(Source::Original, name, kind, name, Status::Original);
    i.category = category;
    i.category_by = CategoryBy::User;
    i
}

fn inv(items: Vec<Item>) -> Inventory {
    let mut inv = Inventory { version: INVENTORY_VERSION, items, ..Default::default() };
    asset_categories::apply(&mut inv);
    inv
}

fn thing_of<'a>(inv: &'a Inventory, name: &str) -> Option<&'a ThingRef> {
    inv.items.iter().find(|i| i.name == name).and_then(|i| i.thing.as_ref())
}

fn role(inv: &Inventory, name: &str) -> Role {
    inv.items.iter().find(|i| i.name == name).and_then(|i| i.role).unwrap()
}

fn enemies() -> Inventory {
    use Kind::{Model as M, Sound as S, Texture as T};
    let e = Category::Enemies;
    inv([
        ("Glyphid_Grunt_A", M),
        ("Glyphid_Grunt_A_001_Decimate_AnimatedMesh_combinedMesh", M),
        ("Glyphid_grunt_A_LOW_GlyphidGrunt_Mat_Normal", T),
        ("Glypid_Grunt_MAT_Base", T),
        ("Glyphid_Slasher", M),
        ("AcidSpitter_001_AnimatedMesh_combinedMesh", M),
        ("AcidSpiter_Mat_Base", T),
        ("Armature_AcidSpitter_Run_AcidSpitter_0", T),
        ("QronarShellback_Model", M),
        ("Shellback_Model_AnimatedMesh_combinedMesh", M),
        ("Shellback_MAT_Base", T),
        ("Shellback_MAT_Masks", T),
        ("Armature_Shellback_SpitLoop_Shellback_2", T),
        ("Exploder_001_AnimatedMesh_combinedMesh", M),
        ("Exploder_Mat_Albedo-Color", T),
        ("FastExploder_001_AnimatedMesh_combinedMesh", M),
        ("FastExploder_Mat_Base", T),
        ("Dreadnought_InGame_RC", M),
        ("Dreadnought_InGame_Twin", M),
        ("Dreadnought_MAT_Masks", T),
        ("Dreadnought-Twin_MAT_Normal", T),
        ("HuuliHoarder_001_AnimatedMesh_combinedMesh", M),
        ("HoarderScaredCombinedA_4", S),
        ("sfx_dread_charge_02", S),
        ("sfx_bugs_small_die_12", S),
        ("sfx_bugs_small_damage_08", S),
        ("AlienFragments_normals", T),
        ("Icon_CocoonSmall", T),
    ]
    .into_iter()
    .map(|(n, k)| item(n, k, e))
    .collect())
}

#[test]
fn roles_come_from_names() {
    let inv = enemies();
    assert_eq!(role(&inv, "Glyphid_Grunt_A"), Role::Model);
    assert_eq!(role(&inv, "Glyphid_grunt_A_LOW_GlyphidGrunt_Mat_Normal"), Role::NormalMap);
    assert_eq!(role(&inv, "Shellback_MAT_Masks"), Role::Mask);
    assert_eq!(role(&inv, "Armature_Shellback_SpitLoop_Shellback_2"), Role::AnimationData);
    assert_eq!(role(&inv, "Glypid_Grunt_MAT_Base"), Role::Colour);
    assert_eq!(role(&inv, "Icon_CocoonSmall"), Role::Icon);
    assert_eq!(role(&inv, "sfx_bugs_small_die_12"), Role::Sound);
    let mut matcap = item("Matcap_Cocoon_Splash", Kind::Texture, Category::Enemies);
    matcap.role = None;
    assert!(role_of(&matcap).technical(), "lookup textures are data, not pictures");
}

#[test]
fn files_join_their_enemy_despite_typos_lods_and_animation_names() {
    let inv = enemies();
    let key = |n: &str| thing_of(&inv, n).map(|t| t.key.clone());
    let grunt = key("Glyphid_Grunt_A");
    assert_eq!(grunt.as_deref(), Some("enemies:glyphidgrunt"));
    for n in ["Glyphid_Grunt_A_001_Decimate_AnimatedMesh_combinedMesh", "Glyphid_grunt_A_LOW_GlyphidGrunt_Mat_Normal", "Glypid_Grunt_MAT_Base"] {
        assert_eq!(key(n), grunt, "{n}");
    }
    assert_ne!(key("Glyphid_Slasher"), grunt, "another glyphid is another enemy");
    let spitter = key("AcidSpitter_001_AnimatedMesh_combinedMesh");
    assert_eq!(key("AcidSpiter_Mat_Base"), spitter, "a typo");
    assert_eq!(key("Armature_AcidSpitter_Run_AcidSpitter_0"), spitter, "animation data");
    // A longer name with no textures of its own is a version of the shorter one…
    let shell = thing_of(&inv, "QronarShellback_Model").unwrap();
    assert_eq!((shell.key.as_str(), shell.name.as_str(), shell.variant.as_deref()), ("enemies:shellback", "Shellback", Some("Qronar")));
    assert_eq!(key("Armature_Shellback_SpitLoop_Shellback_2"), Some("enemies:shellback".into()));
    // …but one with its own textures is a thing of its own.
    assert_ne!(key("FastExploder_001_AnimatedMesh_combinedMesh"), key("Exploder_001_AnimatedMesh_combinedMesh"));
    assert_eq!(key("FastExploder_Mat_Base"), key("FastExploder_001_AnimatedMesh_combinedMesh"));
    // Models that share a stem with a short tag each: one thing, the tags its versions.
    let dread = thing_of(&inv, "Dreadnought_InGame_RC").unwrap();
    assert_eq!((dread.name.as_str(), dread.variant.as_deref()), ("Dreadnought", Some("Rc")));
    assert_eq!(key("Dreadnought_MAT_Masks"), Some(dread.key.clone()));
    assert_eq!(key("Dreadnought-Twin_MAT_Normal"), Some(dread.key.clone()));
    // Sounds: the thing their words name, else a group of their leading words.
    assert_eq!(key("HoarderScaredCombinedA_4"), key("HuuliHoarder_001_AnimatedMesh_combinedMesh"));
    assert_eq!(key("sfx_dread_charge_02"), Some(dread.key.clone()));
    let bugs = thing_of(&inv, "sfx_bugs_small_die_12").unwrap();
    assert!(bugs.sounds_only && bugs.name == "Bugs Small", "{bugs:?}");
    assert_eq!(key("sfx_bugs_small_damage_08"), Some(bugs.key.clone()));
    // A technical file with nothing to join stays in no thing.
    assert_eq!(key("AlienFragments_normals"), None);
}

#[test]
fn enemies_are_counted_as_things_with_models() {
    let inv = enemies();
    let c = counts(&inv, |_| true);
    // Grunt, Slasher, Acid Spitter, Shellback, Exploder, Fast Exploder, Dreadnought, Huuli Hoarder;
    // the cocoon's icon alone isn't an enemy.
    assert_eq!(c.get(&Category::Enemies), Some(&(8, inv.items.len())));
    let (list, loose) = things(inv.items.iter(), Some(Category::Enemies));
    assert_eq!(loose, 1);
    let shell = list.iter().find(|t| t.name == "Shellback").unwrap();
    assert_eq!(shell.roles.get("model"), Some(&2));
    assert_eq!(shell.roles.get("animation_data"), Some(&1));
    assert!(badge(shell).iter().any(|(w, n)| w == "models" && *n == 2));
}

#[test]
fn character_parts_group_into_classes_with_outfits() {
    use Kind::{Model as M, Sound as S, Texture as T};
    let c = Category::Characters;
    let inv = inv([
        ("Driver_BP-Operator_Release_Model", M),
        ("Driver_HG-Operator_Release_Model", M),
        ("Driver_BODY-GridRunner_Release_Model", M),
        ("Driver_BEARD-GridRunner_Release_Model", M),
        ("Driver_GridRunner_HeadGear_MAT_Normal", T),
        ("Driver_Operator_BackPack_MAT_Masks", T),
        ("Engineer_BODY-Default_Release_Model", M),
        ("Engineer_AXE-Default_Tinkerer_Release_Model", M),
        ("Engineer_icon", T),
        ("_Icons_GOLD_CM_Engineer", T),
        ("Gunner_HeavyGunner_BP-Default_Release_Model", M),
        ("SCIENTIST_BODY_Model", M),
        ("Select_ClassMod_Driver_Operator_04", S),
        ("Weapon_Choice_Boomstick_06", S),
        ("Weapon_Choice_Subata_03", S),
    ]
    .into_iter()
    .map(|(n, k)| item(n, k, c))
    .collect());
    let t = |n: &str| thing_of(&inv, n).unwrap().clone();
    let op = t("Driver_BP-Operator_Release_Model");
    assert_eq!((op.key.as_str(), op.name.as_str(), op.variant.as_deref(), op.part.as_deref()), ("characters:driver", "Driver", Some("Operator"), Some("bp")));
    assert_eq!(role(&inv, "Driver_BP-Operator_Release_Model"), Role::ModelPart);
    assert_eq!(t("Driver_BODY-GridRunner_Release_Model").variant.as_deref(), Some("Grid Runner"));
    let map = t("Driver_GridRunner_HeadGear_MAT_Normal");
    assert_eq!((map.key.as_str(), map.variant.as_deref()), ("characters:driver", Some("Grid Runner")), "a part's maps join its class and outfit");
    assert_eq!(t("Engineer_AXE-Default_Tinkerer_Release_Model").variant.as_deref(), Some("Tinkerer"));
    assert_eq!(t("Engineer_icon").key, "characters:engineer");
    assert_eq!(t("_Icons_GOLD_CM_Engineer").key, "characters:engineer");
    assert_eq!(t("Gunner_HeavyGunner_BP-Default_Release_Model").variant.as_deref(), Some("Heavy Gunner"));
    assert_eq!(t("SCIENTIST_BODY_Model").name, "Scientist");
    // Voice lines: the class they name, else a group of their leading words.
    assert_eq!(role(&inv, "Weapon_Choice_Boomstick_06"), Role::VoiceLine);
    assert_eq!(t("Select_ClassMod_Driver_Operator_04").key, "characters:driver");
    let weapons = t("Weapon_Choice_Boomstick_06");
    assert!(weapons.sounds_only && weapons.name == "Weapon Choice");
    assert_eq!(t("Weapon_Choice_Subata_03").key, weapons.key);
    let (list, _) = things(inv.items.iter(), Some(c));
    let driver = list.iter().find(|x| x.name == "Driver").unwrap();
    assert_eq!(driver.variants, ["Grid Runner", "Operator"]);
    assert!(badge(driver).iter().any(|(w, n)| w == "outfits" && *n == 2));
    assert_eq!(counts(&inv, |_| true).get(&c).map(|x| x.0), Some(4), "Driver, Engineer, Gunner, Scientist");
}

#[test]
fn a_person_beats_an_agent_beats_the_rules_and_a_rescan_keeps_them() {
    let mut inv = enemies();
    let id = |inv: &Inventory, n: &str| inv.items.iter().position(|i| i.name == n).unwrap();
    let i = id(&inv, "AlienFragments_normals");
    inv.items[i].thing = Some(ThingRef { key: "enemies:alienfragments".into(), name: "Alien Fragments".into(), variant: None, part: None, sounds_only: false });
    inv.items[i].thing_by = CategoryBy::User;
    let j = id(&inv, "Glyphid_Slasher");
    inv.items[j].thing = thing_of(&inv, "Glyphid_Grunt_A").cloned();
    inv.items[j].thing_by = CategoryBy::Agent;
    inv.thing_names.insert("enemies:glyphidgrunt".into(), "Grunt".into());
    inv.thing_merges.insert("enemies:fastexploder".into(), "enemies:exploder".into());
    asset_categories::apply(&mut inv);
    assert_eq!(thing_of(&inv, "AlienFragments_normals").unwrap().key, "enemies:alienfragments");
    assert_eq!(thing_of(&inv, "Glyphid_Slasher").unwrap().name, "Grunt", "moved by the agent, renamed by the person");
    assert_eq!(thing_of(&inv, "FastExploder_Mat_Base").unwrap().key, "enemies:exploder", "merged");
    // A rescan brings fresh items; the choices stay.
    let fresh: Vec<Item> = enemies()
        .items
        .into_iter()
        .map(|mut x| {
            x.thing = None;
            x.thing_by = CategoryBy::Rule;
            x
        })
        .collect();
    inv.replace(Source::Original, fresh);
    assert_eq!(thing_of(&inv, "AlienFragments_normals").unwrap().key, "enemies:alienfragments");
    assert_eq!(thing_of(&inv, "Glyphid_Slasher").unwrap().name, "Grunt");
    // The project's rules still apply to the rest.
    inv.category_rules.push(CategoryRule { term: "exploder".into(), category: Category::Effects });
    assert!(thing_of(&inv, "Exploder_001_AnimatedMesh_combinedMesh").is_some());
}

#[test]
fn stems_and_typos() {
    assert_eq!(stem("Glyphid_Grunt_A_001_Decimate_AnimatedMesh_combinedMesh", Role::Model).0, ["glyphid", "grunt"]);
    assert_eq!(stem("Armature_Praetorian_Run_Praetorian_0", Role::AnimationData).0, ["praetorian"]);
    assert_eq!(stem("Driver_BP-Operator_Release_Model", Role::ModelPart), (vec!["driver".to_string(), "operator".into()], vec!["bp".to_string()]));
    assert!(same("acidspitter", "acidspiter") && same("glyphid", "glypid"));
    assert!(!same("dreadnought", "dreadnoughtrc"), "a word added isn't a typo");
    assert!(!same("grunt", "brunt") || allowed(5) == 1);
}
