//! What an inventory's files are, as things a person would count: each file gets a technical
//! [`Role`] (a model, a model part, a colour texture, a normal map, a mask, animation data, a
//! sound, a voice line…) and the [`ThingRef`] it belongs to (an enemy, a character class with its
//! outfits, a weapon, a voice group). "Enemies 20 · 851 files", not "Enemies 851".
//!
//! The grouping is generic: no game's names are written here. Names are cut into words
//! (camelCase, `_`, `-`, digits); words that only say what kind of file it is (`mat`, `base`,
//! `normal`, `model`, `release`, `animatedmesh`…) are dropped; part words (`body`, `hg`,
//! `backpack`, `beard`, `axe`…) say a model part; animation words in `Armature_*` vertex
//! animation textures are dropped. What's left (the stem) is matched to the stems of the
//! category's models first ("anchors"), allowing a typo or two (`AcidSpiter` ↔ `AcidSpitter`,
//! `Glypid` ↔ `Glyphid`) and a longer name holding a shorter one (`Qronar Shellback` ↔
//! `Shellback`). Model parts group by their first word (a character class), the rest of the
//! stem being the outfit. Colour textures and icons with no model are things of their own;
//! technical files (normal maps, masks, alpha, animation data, effect meshes) never are: they
//! join a thing or stay ungrouped. Sounds join the thing their words name, else a group of
//! their leading words.
//!
//! The person's choice wins over an agent's, and both over the rules; a rescan keeps them
//! ([`crate::asset_categories::CategoryBy`]), and the project's renames and merges
//! ([`Inventory::thing_names`], [`Inventory::thing_merges`]) apply after the rules.

use crate::asset_categories::{Category, CategoryBy, words};
use crate::inventory::{Inventory, Item, Kind};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// What a file is, technically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Model,
    /// A piece of a model (a body, a helmet, a backpack, a beard, a tool).
    ModelPart,
    /// A colour (albedo, base colour) texture or sprite.
    Colour,
    NormalMap,
    /// Channels saying where effects apply (metal, roughness, emission…).
    Mask,
    Alpha,
    /// Positions or bones baked into a texture (`Armature_*`), not a picture.
    AnimationData,
    /// A mesh for an effect (an emitter, particles).
    EffectMesh,
    /// An icon or other interface art.
    Icon,
    Sound,
    VoiceLine,
    Music,
    Font,
    Video,
    Other,
}

impl Role {
    pub fn id(self) -> &'static str {
        match self {
            Role::Model => "model",
            Role::ModelPart => "model_part",
            Role::Colour => "colour",
            Role::NormalMap => "normal_map",
            Role::Mask => "mask",
            Role::Alpha => "alpha",
            Role::AnimationData => "animation_data",
            Role::EffectMesh => "effect_mesh",
            Role::Icon => "icon",
            Role::Sound => "sound",
            Role::VoiceLine => "voice_line",
            Role::Music => "music",
            Role::Font => "font",
            Role::Video => "video",
            Role::Other => "other",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Role::Model => "Models",
            Role::ModelPart => "Model parts",
            Role::Colour => "Colour textures",
            Role::NormalMap => "Normal maps",
            Role::Mask => "Masks",
            Role::Alpha => "Alpha masks",
            Role::AnimationData => "Animation data",
            Role::EffectMesh => "Effect meshes",
            Role::Icon => "Icons and interface art",
            Role::Sound => "Sounds",
            Role::VoiceLine => "Voice lines",
            Role::Music => "Music",
            Role::Font => "Fonts",
            Role::Video => "Videos",
            Role::Other => "Other files",
        }
    }

    /// Made with a model, never a thing (or a generation) of its own.
    pub fn technical(self) -> bool {
        matches!(self, Role::NormalMap | Role::Mask | Role::Alpha | Role::AnimationData | Role::EffectMesh)
    }

    /// Something to look at: a thing can be made of it.
    pub fn visual(self) -> bool {
        matches!(self, Role::Model | Role::ModelPart | Role::Colour | Role::Icon)
    }

    pub fn audio(self) -> bool {
        matches!(self, Role::Sound | Role::VoiceLine | Role::Music)
    }

    pub fn from_id(id: &str) -> Option<Role> {
        ROLES.iter().copied().find(|r| r.id() == id)
    }
}

pub const ROLES: &[Role] = &[
    Role::Model,
    Role::ModelPart,
    Role::Colour,
    Role::NormalMap,
    Role::Mask,
    Role::Alpha,
    Role::AnimationData,
    Role::EffectMesh,
    Role::Icon,
    Role::Sound,
    Role::VoiceLine,
    Role::Music,
    Role::Font,
    Role::Video,
    Role::Other,
];

/// The thing a file belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ThingRef {
    /// Stable within the project: the category and the thing's stem (`enemies:glyphidgrunt`).
    pub key: String,
    /// For people: `Glyphid Grunt`.
    pub name: String,
    /// An outfit or version (`Grid Runner`), when the thing has several.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    /// The part of the model this file is (`body`, `backpack`), for model parts and their maps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub part: Option<String>,
    /// A group of sounds or voice lines with no model or picture.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sounds_only: bool,
}

// ---- Words ----

/// Words that only say what kind of file it is.
const ROLE_WORDS: &[&str] = &[
    "mat",
    "material",
    "base",
    "albedo",
    "color",
    "colour",
    "diffuse",
    "basecolor",
    "normal",
    "normals",
    "nrm",
    "nor",
    "mask",
    "masks",
    "alpha",
    "opacity",
    "model",
    "mesh",
    "release",
    "default",
    "animatedmesh",
    "animated",
    "combinedmesh",
    "combined",
    "decimate",
    "decimated",
    "lod",
    "low",
    "high",
    "tex",
    "texture",
    "textures",
    "icon",
    "icons",
    "emitter",
    "emitters",
    "prefab",
    "geo",
    "ingame",
    "in",
    "game",
    "the",
    "of",
    "and",
    "sfx",
    "sm",
    "sk",
    "t",
    "m",
    "cm",
    "fbx",
    "obj",
    "png",
    "wav",
    "ogg",
    "final",
    "new",
    "old",
    "copy",
];
/// Words naming a part of a model.
const PART_WORDS: &[&str] =
    &["body", "hg", "headgear", "helmet", "head", "gear", "bp", "backpack", "back", "pack", "pad", "packpad", "beard", "axe", "drill", "drills", "hair"];
/// Words of textures that are lookup data, not pictures of a thing.
const DATA_WORDS: &[&str] = &["matcap", "splatmap", "lut", "ramp", "gradient", "flowmap", "noise", "curve"];
/// Words of an animation's name in a vertex-animation texture.
const ANIM_WORDS: &[&str] = &[
    "run",
    "idle",
    "attack",
    "death",
    "die",
    "dead",
    "fold",
    "unfold",
    "walk",
    "spit",
    "spitloop",
    "spitstart",
    "spitend",
    "loop",
    "start",
    "end",
    "hit",
    "hurt",
    "jump",
    "fly",
    "flying",
    "land",
    "spawn",
    "burrow",
    "unburrow",
    "roar",
    "turn",
    "charge",
    "dash",
    "stun",
    "fall",
    "anim",
    "animation",
];

/// The kind words of a texture's role, in the order they're checked.
fn texture_role(w: &[String], name_lower: &str, category: Category) -> Role {
    let has = |x: &str| w.iter().any(|t| t == x);
    if name_lower.starts_with("armature_") || has("vat") || has("vertexanim") {
        Role::AnimationData
    } else if has("normal") || has("normals") || has("nrm") || has("nor") || w.last().is_some_and(|t| t == "n") {
        Role::NormalMap
    } else if has("mask") || has("masks") || DATA_WORDS.iter().any(|d| has(d)) {
        Role::Mask
    } else if has("alpha") || has("opacity") {
        Role::Alpha
    } else if has("icon") || has("icons") || has("ui") || has("hud") || has("button") || has("cursor") || has("logo") || category == Category::Hud {
        Role::Icon
    } else {
        Role::Colour
    }
}

/// An item's role by the rules.
pub fn role_of(item: &Item) -> Role {
    let w = words(&item.name);
    let lower = item.name.to_ascii_lowercase();
    match item.kind {
        Kind::Model => {
            if w.iter().any(|t| matches!(t.as_str(), "emitter" | "emitters" | "particle" | "particles" | "vfx" | "fx")) {
                Role::EffectMesh
            } else if w.iter().any(|t| PART_WORDS.contains(&t.as_str())) {
                Role::ModelPart
            } else {
                Role::Model
            }
        }
        Kind::Texture | Kind::Sprite => texture_role(&w, &lower, item.category),
        Kind::Sound if item.category == Category::Characters && !w.iter().any(|t| t == "sfx" || t == "ui") => Role::VoiceLine,
        Kind::Sound => Role::Sound,
        Kind::Music => Role::Music,
        Kind::Font => Role::Font,
        Kind::Video => Role::Video,
        _ => Role::Other,
    }
}

/// A name's stem words (kind, part and animation words, digits and single letters left out),
/// and its part words.
pub fn stem(name: &str, role: Role) -> (Vec<String>, Vec<String>) {
    let mut out = Vec::new();
    let mut parts = Vec::new();
    for w in words(name) {
        let w = w.trim_end_matches(|c: char| c.is_ascii_digit()).to_string();
        if w.len() <= 1 || ROLE_WORDS.contains(&w.as_str()) {
            continue;
        }
        if role == Role::AnimationData && (ANIM_WORDS.contains(&w.as_str()) || w == "armature") {
            continue;
        }
        if PART_WORDS.contains(&w.as_str()) && !matches!(role, Role::Sound | Role::VoiceLine | Role::Music) {
            if !parts.contains(&w) {
                parts.push(w);
            }
            continue;
        }
        // The same word twice in a row (`Mactera-Spawn_Low_Mactera-Spawn`) says nothing more.
        out.push(w);
    }
    // A name that repeats itself (`Praetorian_Run_Praetorian`): once.
    let half = out.len() / 2;
    if half > 0 && out.len() % 2 == 0 && out[..half] == out[half..] {
        out.truncate(half);
    }
    (out, parts)
}

fn join(w: &[String]) -> String {
    w.concat()
}

fn title(w: &[String]) -> String {
    w.iter()
        .map(|t| {
            let mut c = t.chars();
            c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Edit distance, bounded (early out when over `max`).
fn distance(a: &str, b: &str, max: usize) -> Option<usize> {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a.len().abs_diff(b.len()) > max {
        return None;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            cur[j + 1] = (prev[j] + usize::from(ca != cb)).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        if cur.iter().min().copied().unwrap_or(0) > max {
            return None;
        }
        prev = cur;
    }
    prev.last().copied().filter(|d| *d <= max)
}

/// How many letters two stems may differ by and still be one thing.
fn allowed(len: usize) -> usize {
    match len {
        0..=4 => 0,
        5..=8 => 1,
        _ => 2,
    }
}

/// Two stems that are the same thing, give or take a typo (a letter more, less or different
/// here and there; not a word added).
pub fn same(a: &str, b: &str) -> bool {
    a == b || (a.len().abs_diff(b.len()) <= 1 && distance(a, b, allowed(a.len().min(b.len()))).is_some())
}

// ---- Grouping ----

/// One thing being built: its stem, its display words, and the variants seen.
#[derive(Debug, Clone)]
struct Anchor {
    stem: String,
    words: Vec<String>,
    /// Made of model parts: a class whose variants are outfits.
    class: bool,
}

/// Which anchor a stem belongs to: the same (give or take a typo), one it starts with, the
/// longest one it holds, or the only one that starts with it.
fn find(anchors: &[Anchor], s: &str) -> Option<usize> {
    if s.is_empty() {
        return None;
    }
    if let Some(i) = anchors.iter().position(|a| same(&a.stem, s)) {
        return Some(i);
    }
    let longest = |it: &mut dyn Iterator<Item = (usize, &Anchor)>| it.max_by_key(|(_, a)| a.stem.len()).map(|(i, _)| i);
    if let Some(i) = longest(&mut anchors.iter().enumerate().filter(|(_, a)| s.starts_with(&a.stem))) {
        return Some(i);
    }
    // Starts with one, give or take a typo.
    if let Some(i) = longest(
        &mut anchors
            .iter()
            .enumerate()
            .filter(|(_, a)| a.stem.len() >= 5 && s.len() > a.stem.len() && s.get(..a.stem.len()).is_some_and(|p| distance(p, &a.stem, 1).is_some())),
    ) {
        return Some(i);
    }
    let held: Vec<usize> = anchors.iter().enumerate().filter(|(_, a)| a.stem.len() >= 5 && s.contains(&a.stem)).map(|(i, _)| i).collect();
    if let Some(i) = held.iter().copied().max_by_key(|i| anchors[*i].stem.len()) {
        let len = anchors[i].stem.len();
        if held.iter().filter(|j| anchors[**j].stem.len() == len).count() == 1 {
            return Some(i);
        }
    }
    let longer: Vec<usize> = anchors.iter().enumerate().filter(|(_, a)| s.len() >= 5 && a.stem.starts_with(s)).map(|(i, _)| i).collect();
    (longer.len() == 1).then(|| longer[0])
}

/// What one item contributes: its role, stem words, part words.
struct Look {
    role: Role,
    words: Vec<String>,
    parts: Vec<String>,
}

/// Group every item the rules decide (role and thing), category by category.
pub fn group(inv: &mut Inventory) {
    let looks: Vec<Look> = inv
        .items
        .iter()
        .map(|i| {
            let role = if i.role_by == CategoryBy::Rule { role_of(i) } else { i.role.unwrap_or_else(|| role_of(i)) };
            let (words, parts) = stem(&i.name, role);
            Look { role, words, parts }
        })
        .collect();
    let categories: BTreeSet<Category> = inv.items.iter().map(|i| i.category).collect();
    let mut things: Vec<Option<ThingRef>> = vec![None; inv.items.len()];
    for cat in categories {
        let idx: Vec<usize> = (0..inv.items.len()).filter(|i| inv.items[*i].category == cat).collect();
        let mut anchors: Vec<Anchor> = Vec::new();
        let add = |anchors: &mut Vec<Anchor>, words: Vec<String>, class: bool| -> usize {
            let stem = join(&words);
            if let Some(i) = anchors.iter().position(|a| a.stem == stem) {
                anchors[i].class |= class;
                return i;
            }
            anchors.push(Anchor { stem, words, class });
            anchors.len() - 1
        };
        // 1. Models: whole models by their stem; parts by their first word (a class).
        for &i in &idx {
            let l = &looks[i];
            match l.role {
                Role::Model if !l.words.is_empty() => {
                    add(&mut anchors, l.words.clone(), false);
                }
                Role::ModelPart if !l.words.is_empty() => {
                    add(&mut anchors, l.words[..1].to_vec(), true);
                }
                _ => {}
            }
        }
        // Colour textures' stems (in any category): a model with one of its own is a thing of its own.
        let colours: BTreeSet<String> = looks.iter().filter(|l| l.role == Role::Colour).map(|l| join(&l.words)).collect();
        let own_colour = |stem: &str| colours.iter().any(|c| same(c, stem));
        // A longer model name holding a shorter one, without a colour texture of its own, or the
        // same give or take a typo: one thing (`Qronar Shellback` → `Shellback`, `Mactera Spawn
        // Smaller Wings` → `Mactera Spawn`; `Fast Exploder`, with its own textures, stays).
        let mut merged: BTreeMap<usize, usize> = BTreeMap::new();
        let mut variant_of: BTreeMap<usize, Vec<String>> = BTreeMap::new();
        for a in 0..anchors.len() {
            let target = (0..anchors.len()).filter(|b| *b != a && !anchors[a].class && !anchors[*b].class).find(|b| {
                let (x, y) = (&anchors[a].stem, &anchors[*b].stem);
                (y.len() < x.len() && y.len() >= 5 && x.contains(y.as_str()) && !own_colour(x)) || (x.len() >= y.len() && a > *b && same(x, y))
            });
            if let Some(b) = target {
                let extra: Vec<String> = anchors[a].words.iter().filter(|w| !anchors[b].words.contains(w)).cloned().collect();
                if !extra.is_empty() {
                    variant_of.insert(a, extra);
                }
                merged.insert(a, b);
            }
        }
        // A stem the other files use that starts several models, each with a short tag after it
        // (`Dreadnought` → `Dreadnought RC`, `Dreadnought Twin`): one thing, the tags its variants.
        let used: BTreeSet<Vec<String>> = idx.iter().map(|i| looks[*i].words.clone()).filter(|w| join(w).len() >= 5).collect();
        for prefix in used {
            let p = join(&prefix);
            if anchors.iter().any(|a| a.stem == p) {
                continue;
            }
            let tagged: Vec<usize> = (0..anchors.len())
                .filter(|a| !anchors[*a].class && !merged.contains_key(a))
                .filter(|a| anchors[*a].words.len() == prefix.len() + 1 && anchors[*a].words.starts_with(&prefix))
                .filter(|a| anchors[*a].words.last().is_some_and(|t| t.len() <= 4))
                .collect();
            if tagged.len() >= 2 {
                anchors.push(Anchor { stem: p, words: prefix.clone(), class: false });
                let to = anchors.len() - 1;
                for a in tagged {
                    variant_of.insert(a, anchors[a].words[prefix.len()..].to_vec());
                    merged.insert(a, to);
                }
            }
        }
        let resolve = |mut a: usize| {
            let mut hops = 0;
            while let Some(&b) = merged.get(&a) {
                a = b;
                hops += 1;
                if hops > 16 {
                    break;
                }
            }
            a
        };
        let base_anchor_count = anchors.len();
        // 2. Colour textures and icons that match no model: things of their own.
        for &i in &idx {
            let l = &looks[i];
            if matches!(l.role, Role::Colour | Role::Icon) && !l.words.is_empty() {
                let s = join(&l.words);
                if find(&anchors[..base_anchor_count], &s).is_none() && find(&anchors, &s).is_none() {
                    add(&mut anchors, l.words.clone(), false);
                }
            }
        }
        // 3. Every item to its anchor.
        for &i in &idx {
            let l = &looks[i];
            let s = join(&l.words);
            let anchor = match l.role {
                // A part's class is its first word.
                Role::ModelPart => l.words.first().and_then(|w| anchors.iter().position(|a| a.class && &a.stem == w)),
                _ if l.role.audio() => audio_anchor(&anchors, base_anchor_count, &l.words, &resolve),
                _ => {
                    // Maps and textures of a part: the class first, when there's one.
                    let class = (!l.parts.is_empty()).then(|| l.words.first().and_then(|w| anchors.iter().position(|a| a.class && &a.stem == w))).flatten();
                    class.or_else(|| find(&anchors, &s))
                }
            };
            match anchor {
                Some(found) => {
                    let a = resolve(found);
                    let an = &anchors[a];
                    // A class's outfit: the words after the class's own name.
                    let variant_words: Vec<String> = if an.class && l.words.first().is_some_and(|w| *w == an.stem) && l.role != Role::Icon {
                        l.words.iter().skip(1).filter(|w| !matches!(w.as_str(), "gold" | "golden" | "illu" | "portrait")).cloned().collect()
                    } else {
                        variant_of.get(&found).cloned().unwrap_or_default()
                    };
                    things[i] = Some(ThingRef {
                        key: format!("{}:{}", cat.id(), an.stem),
                        name: title(&an.words),
                        variant: (!variant_words.is_empty()).then(|| title(&variant_words)),
                        part: l.parts.first().cloned(),
                        sounds_only: false,
                    });
                }
                None if l.role.audio() => {
                    // A group of its leading words.
                    let lead: Vec<String> = l.words.iter().take(2).cloned().collect();
                    if !lead.is_empty() {
                        things[i] = Some(ThingRef {
                            key: format!("{}:sounds:{}", cat.id(), join(&lead)),
                            name: title(&lead),
                            variant: None,
                            part: None,
                            sounds_only: true,
                        });
                    }
                }
                None => {}
            }
        }
    }
    // The rules' choices, the project's renames and merges, and the choices people and agents
    // made (kept).
    for (n, item) in inv.items.iter_mut().enumerate() {
        if item.role_by == CategoryBy::Rule {
            item.role = Some(looks[n].role);
        }
        if item.thing_by == CategoryBy::Rule {
            item.thing = things[n].take();
        }
        if let Some(t) = item.thing.as_mut() {
            let mut hops = 0;
            while let Some(into) = inv.thing_merges.get(&t.key) {
                t.key.clone_from(into);
                hops += 1;
                if hops > 16 {
                    break;
                }
            }
            if let Some(name) = inv.thing_names.get(&t.key) {
                t.name.clone_from(name);
            }
        }
    }
    // A thing a merge left without a name of its own takes the name of the thing it went into.
    let names: BTreeMap<String, String> =
        inv.items.iter().filter_map(|i| i.thing.as_ref()).filter(|t| !inv.thing_merges.contains_key(&t.key)).map(|t| (t.key.clone(), t.name.clone())).collect();
    for t in inv.items.iter_mut().filter_map(|i| i.thing.as_mut()) {
        if let Some(n) = names.get(&t.key) {
            t.name.clone_from(n);
        }
    }
}

/// A sound's thing: the longest anchor its words name; else the one thing a word of it names
/// the end or the start of (`hoarder` → `Huuli Hoarder`, `dread` → `Dreadnought`).
fn audio_anchor(anchors: &[Anchor], models: usize, w: &[String], resolve: &dyn Fn(usize) -> usize) -> Option<usize> {
    let all = join(w);
    let named = anchors
        .iter()
        .enumerate()
        .filter(|(_, a)| a.stem.len() >= 4 && (all.contains(&a.stem) || w.iter().any(|t| t.len() >= 5 && same(t, &a.stem))))
        .max_by_key(|(_, a)| a.stem.len())
        .map(|(i, _)| i);
    if named.is_some() {
        return named;
    }
    // Only things with a model, and only telling words.
    let near: BTreeSet<usize> = anchors
        .iter()
        .take(models)
        .enumerate()
        .filter(|(_, a)| {
            w.iter().any(|t| {
                (t.len() >= 6 && a.words.len() > 1 && a.words.last().is_some_and(|l| l == t))
                    || (t.len() >= 5 && a.words.first().is_some_and(|f| f.len() > t.len() && f.starts_with(t.as_str())))
            })
        })
        .map(|(i, _)| resolve(i))
        .collect();
    (near.len() == 1).then(|| near.into_iter().next()).flatten()
}

// ---- Counting ----

// ---- Before and after ----

/// Where a thing's new version stands, from asked for to in the game (the order counts: a
/// thing shows its furthest).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NewVersion {
    /// Sent to be rebuilt; its new version isn't there yet.
    BeingMade,
    /// Made (by local AI or the builder), not yet in the game.
    Made,
    /// Replaced in the game.
    InTheGame,
}

impl NewVersion {
    pub fn id(self) -> &'static str {
        match self {
            NewVersion::BeingMade => "being_made",
            NewVersion::Made => "made",
            NewVersion::InTheGame => "in_the_game",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            NewVersion::BeingMade => "Being made",
            NewVersion::Made => "Made",
            NewVersion::InTheGame => "In the game",
        }
    }
}

/// Where an item's new version stands, when it has one.
pub fn new_version(i: &Item) -> Option<NewVersion> {
    use crate::inventory::Status;
    match i.status {
        Status::Replaced => Some(NewVersion::InTheGame),
        Status::Generated => Some(NewVersion::Made),
        Status::NeedsReplacing if i.rebuild.is_some() => Some(NewVersion::BeingMade),
        _ => None,
    }
}

/// The picture of an item's new version (relative to the project): the generated picture, or
/// a generated model's preview (`<model>_preview.png` beside it); for a generated asset listed on
/// its own, its own picture.
pub fn new_picture(i: &Item) -> Option<String> {
    let picture = |p: &str| {
        let lower = p.to_ascii_lowercase();
        if [".png", ".jpg", ".jpeg", ".webp"].iter().any(|e| lower.ends_with(e)) {
            return Some(p.to_string());
        }
        // A model: the preview written beside it.
        [".glb", ".gltf", ".fbx", ".obj"]
            .iter()
            .find_map(|e| lower.ends_with(e).then(|| p.get(..p.len() - e.len())).flatten())
            .map(|stem| format!("{stem}_preview.png"))
    };
    if let Some(out) = i.rebuild.as_ref().and_then(|r| r.output.as_deref()) {
        return picture(out);
    }
    // A generated file listed as an item of its own (assets.json): it is the new version.
    (i.status == crate::inventory::Status::Generated && i.source == crate::inventory::Source::Build)
        .then(|| i.thumb.clone().or_else(|| i.path.as_deref().and_then(picture)))
        .flatten()
}

/// One thing, as the gallery and the tools show it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Thing {
    pub key: String,
    pub name: String,
    pub category: Category,
    /// Files by role id.
    pub roles: BTreeMap<String, usize>,
    pub files: usize,
    /// Its outfits or versions.
    pub variants: Vec<String>,
    /// Its picture: a model's preview, else a colour texture's, else an icon's (relative to the
    /// project).
    pub preview: Option<String>,
    /// The item the picture is of.
    pub preview_item: Option<String>,
    /// Only sounds or voice lines.
    pub sounds_only: bool,
    /// It has a model or model parts.
    pub has_model: bool,
    /// Grouped by a person or an agent (some of its files).
    pub chosen: bool,
    /// Where its new version stands (the furthest of its files), when it has one.
    #[serde(default)]
    pub after: Option<NewVersion>,
    /// The new version's picture (relative to the project), and the item it's of.
    #[serde(default)]
    pub after_preview: Option<String>,
    #[serde(default)]
    pub after_item: Option<String>,
    /// The rebuild jobs its new version came from.
    #[serde(default)]
    pub jobs: Vec<String>,
}

/// The things of `items` (in `category`, or all), biggest first; and the files in no thing.
pub fn things<'a>(items: impl Iterator<Item = &'a Item>, category: Option<Category>) -> (Vec<Thing>, usize) {
    let mut map: BTreeMap<String, Thing> = BTreeMap::new();
    let mut loose = 0;
    let mut preview_rank: BTreeMap<String, u8> = BTreeMap::new();
    for i in items.filter(|i| category.is_none_or(|c| i.category == c)) {
        let Some(t) = &i.thing else {
            loose += 1;
            continue;
        };
        let th = map.entry(t.key.clone()).or_insert_with(|| Thing {
            key: t.key.clone(),
            name: t.name.clone(),
            category: i.category,
            roles: BTreeMap::new(),
            files: 0,
            variants: Vec::new(),
            preview: None,
            preview_item: None,
            sounds_only: true,
            has_model: false,
            chosen: false,
            after: None,
            after_preview: None,
            after_item: None,
            jobs: Vec::new(),
        });
        let role = i.role.unwrap_or_else(|| role_of(i));
        *th.roles.entry(role.id().to_string()).or_default() += 1;
        th.files += 1;
        th.sounds_only &= t.sounds_only;
        th.has_model |= matches!(role, Role::Model | Role::ModelPart);
        th.chosen |= i.thing_by != CategoryBy::Rule;
        if let Some(v) = &t.variant
            && !th.variants.contains(v)
        {
            th.variants.push(v.clone());
        }
        let rank = match role {
            Role::Model => 4,
            Role::ModelPart if t.part.as_deref() == Some("body") => 4,
            Role::ModelPart => 3,
            Role::Colour => 2,
            Role::Icon => 1,
            _ => 0,
        };
        if rank > 0 && i.thumb.is_some() && preview_rank.get(&t.key).is_none_or(|r| rank > *r) {
            preview_rank.insert(t.key.clone(), rank);
            th.preview.clone_from(&i.thumb);
            th.preview_item = Some(i.id.clone());
        }
        if let Some(nv) = new_version(i) {
            th.after = th.after.max(Some(nv));
            if let Some(job) = i.rebuild.as_ref().map(|r| r.job.clone()).filter(|j| !j.is_empty() && !th.jobs.contains(j)) {
                th.jobs.push(job);
            }
            if let Some(p) = new_picture(i) {
                let key = format!("after:{}", t.key);
                if preview_rank.get(&key).is_none_or(|r| rank.max(1) > *r) {
                    preview_rank.insert(key, rank.max(1));
                    th.after_preview = Some(p);
                    th.after_item = Some(i.id.clone());
                }
            }
        }
    }
    let mut v: Vec<Thing> = map.into_values().collect();
    for t in &mut v {
        t.variants.sort();
    }
    // Things with a model first, then pictures, then sound groups; bigger first.
    v.sort_by(|a, b| a.sounds_only.cmp(&b.sounds_only).then(b.has_model.cmp(&a.has_model)).then(b.files.cmp(&a.files)).then(a.name.cmp(&b.name)));
    (v, loose)
}

/// Whether a thing counts in its category: in a category of 3D things (characters, enemies,
/// weapons, items, machines, the environment) that has models, the things with one (an icon or
/// a texture alone doesn't make an enemy); elsewhere every thing with a picture.
pub fn counted(t: &Thing, category_has_models: bool) -> bool {
    let by_model = category_has_models && !matches!(t.category, Category::Hud | Category::Effects | Category::Unsorted | Category::Music);
    !t.sounds_only && (t.has_model || !by_model)
}

/// How many things and files each category has (see [`counted`]).
pub fn counts(inv: &Inventory, visible: impl Fn(&Item) -> bool) -> BTreeMap<Category, (usize, usize)> {
    let mut files: BTreeMap<Category, usize> = BTreeMap::new();
    for i in inv.items.iter().filter(|i| visible(i)) {
        *files.entry(i.category).or_default() += 1;
    }
    let (all, _) = things(inv.items.iter().filter(|i| visible(i)), None);
    let n = count_things(&all);
    files.into_iter().map(|(c, f)| (c, (n.get(&c).copied().unwrap_or(0), f))).collect()
}

/// The things that count ([`counted`]) and have a new version, in each category of `list`.
pub fn count_after(list: &[Thing]) -> BTreeMap<Category, usize> {
    let with_models: BTreeSet<Category> = list.iter().filter(|t| t.has_model).map(|t| t.category).collect();
    let mut n: BTreeMap<Category, usize> = BTreeMap::new();
    for t in list.iter().filter(|t| t.after.is_some()) {
        if counted(t, with_models.contains(&t.category)) {
            *n.entry(t.category).or_default() += 1;
        }
    }
    n
}

/// The things that count ([`counted`]) in each category of `list`.
pub fn count_things(list: &[Thing]) -> BTreeMap<Category, usize> {
    let with_models: BTreeSet<Category> = list.iter().filter(|t| t.has_model).map(|t| t.category).collect();
    let mut n: BTreeMap<Category, usize> = BTreeMap::new();
    for t in list {
        if counted(t, with_models.contains(&t.category)) {
            *n.entry(t.category).or_default() += 1;
        }
    }
    n
}

/// What a category's things are called: "enemies", "characters"…
pub fn noun(category: Category) -> &'static str {
    match category {
        Category::Characters => "characters",
        Category::Enemies => "enemies",
        Category::Weapons => "weapons",
        Category::Items => "items",
        Category::Machines => "machines",
        Category::Environment => "pieces",
        Category::Hud => "pictures",
        Category::Effects => "effects",
        Category::Music => "pieces of music",
        _ => "things",
    }
}

/// A short line of what a thing holds: "3 outfits · 2 models · 6 textures · 34 sounds".
pub fn badge(t: &Thing) -> Vec<(String, usize)> {
    let get = |r: Role| t.roles.get(r.id()).copied().unwrap_or(0);
    let textures = get(Role::Colour) + get(Role::NormalMap) + get(Role::Mask) + get(Role::Alpha) + get(Role::Icon);
    let mut out = Vec::new();
    if t.variants.len() > 1 {
        out.push(("outfits".to_string(), t.variants.len()));
    }
    for (what, n) in [
        ("models", get(Role::Model)),
        ("parts", get(Role::ModelPart)),
        ("textures", textures),
        ("animation", get(Role::AnimationData)),
        ("sounds", get(Role::Sound) + get(Role::Music)),
        ("voice lines", get(Role::VoiceLine)),
    ] {
        if n > 0 {
            out.push((what.to_string(), n));
        }
    }
    out
}

#[cfg(test)]
mod tests;
