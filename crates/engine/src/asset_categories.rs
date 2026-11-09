//! What each asset is for in the game (its category: characters, enemies, the HUD…), so the
//! person can see what to rebuild and rebuild one group at a time.
//!
//! An item's category comes from, in order of precedence: the person (`user`), an agent
//! (`agent`), then rules (`rule`): the project's own terms ([`CategoryRule`], kept in
//! inventory.json, so project-specific names sort without hard-coding them here), then the
//! built-in keywords ([`KEYWORDS`]), then the item's kind ([`kind_default`]). A rescan keeps
//! what the person and agents chose and applies the rules again to the rest.
//!
//! Matching works on words: a name such as `AcidSpitter_Body-LOD0` becomes `acid spitter body
//! lod0` (split on `_`, `-`, spaces, other punctuation and camelCase). A keyword matches a word
//! that equals it, its plural, or (for keywords of four letters or more) begins with it.

use crate::inventory::{Inventory, Item, Kind};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

/// What an asset is for in the game.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    /// The player and their gear and voice.
    Characters,
    Enemies,
    /// Weapons and tools.
    Weapons,
    /// Items and pickups.
    Items,
    /// Machines and props: vehicles, turrets, pods, crates.
    Machines,
    /// HUD and menus: icons, buttons, fonts, menu and UI sounds, menu video.
    Hud,
    /// Terrain, rocks, caves, sky, ambience.
    Environment,
    /// Particles, emitters, trails, impacts.
    Effects,
    Music,
    /// Not sorted yet.
    #[default]
    Unsorted,
}

impl Category {
    pub const ALL: [Category; 10] = [
        Category::Characters,
        Category::Enemies,
        Category::Weapons,
        Category::Items,
        Category::Machines,
        Category::Hud,
        Category::Environment,
        Category::Effects,
        Category::Music,
        Category::Unsorted,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Category::Characters => "characters",
            Category::Enemies => "enemies",
            Category::Weapons => "weapons",
            Category::Items => "items",
            Category::Machines => "machines",
            Category::Hud => "hud",
            Category::Environment => "environment",
            Category::Effects => "effects",
            Category::Music => "music",
            Category::Unsorted => "unsorted",
        }
    }

    /// Its name for people (English; the app translates it).
    pub fn title(self) -> &'static str {
        match self {
            Category::Characters => "Characters",
            Category::Enemies => "Enemies",
            Category::Weapons => "Weapons & tools",
            Category::Items => "Items & pickups",
            Category::Machines => "Machines & props",
            Category::Hud => "HUD & menus",
            Category::Environment => "Environment",
            Category::Effects => "Effects",
            Category::Music => "Music",
            Category::Unsorted => "Unsorted",
        }
    }

    pub fn from_id(id: &str) -> Option<Category> {
        Category::ALL.into_iter().find(|c| c.id() == id)
    }
}

/// Who chose an item's category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CategoryBy {
    /// The rules (the project's terms, the built-in keywords, the kind); applied again on rescans.
    #[default]
    Rule,
    /// An agent (project_inventory_categorize); kept on rescans.
    Agent,
    /// The person; kept on rescans, and an agent's choice doesn't replace it.
    User,
}

/// A project's own term: items whose name, path or origin has it go to `category`. Wins over
/// the built-in keywords.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CategoryRule {
    /// A word (`glyphid`) or words (`bugs small`), matched like the built-in keywords; any case.
    pub term: String,
    pub category: Category,
}

/// The built-in keywords: generic words for what game assets are, and their category. The first
/// word of a name that matches decides.
pub const KEYWORDS: &[(Category, &[&str])] = &[
    (
        Category::Hud,
        &[
            "ui",
            "hud",
            "icon",
            "button",
            "btn",
            "menu",
            "cursor",
            "font",
            "widget",
            "banner",
            "window",
            "logo",
            "crosshair",
            "reticle",
            "tooltip",
            "select",
            "shop",
            "gamepad",
            "controller",
            "keyboard",
            "prompt",
            "hover",
            "slider",
            "checkbox",
            "popup",
            "dialog",
            "screen",
            "sdf",
        ],
    ),
    (Category::Enemies, &["enemy", "enemies", "boss", "monster", "zombie", "creature", "bug", "alien", "mob", "spider", "insect", "beast", "swarm"]),
    (
        Category::Weapons,
        &[
            "weapon",
            "gun",
            "rifle",
            "pistol",
            "shotgun",
            "grenade",
            "ammo",
            "projectile",
            "bullet",
            "muzzle",
            "reload",
            "shot",
            "shoot",
            "laser",
            "sword",
            "axe",
            "pickaxe",
            "drill",
            "hammer",
            "bow",
            "arrow",
            "rocket",
            "missile",
            "launcher",
            "minigun",
            "flamethrower",
            "knife",
            "blade",
            "tool",
        ],
    ),
    (Category::Items, &["pickup", "loot", "coin", "gem", "ore", "mineral", "health", "potion", "crystal", "gold", "treasure", "powerup", "collectible", "xp"]),
    (
        Category::Machines,
        &[
            "vehicle",
            "turret",
            "crate",
            "barrel",
            "door",
            "pod",
            "car",
            "truck",
            "machine",
            "robot",
            "drone",
            "elevator",
            "prop",
            "container",
            "generator",
            "mech",
        ],
    ),
    (
        Category::Environment,
        &[
            "ground",
            "terrain",
            "rock",
            "wall",
            "floor",
            "cave",
            "sky",
            "tree",
            "grass",
            "water",
            "amb",
            "ambience",
            "ambient",
            "footstep",
            "stone",
            "dirt",
            "sand",
            "mud",
            "lava",
            "magma",
            "ice",
            "snow",
            "cliff",
            "mountain",
            "level",
            "brick",
            "wood",
            "leaf",
            "foliage",
            "plant",
            "bush",
            "cloud",
            "wind",
            "rain",
            "environment",
            "env",
        ],
    ),
    (
        Category::Effects,
        &[
            "fx",
            "vfx",
            "particle",
            "emitter",
            "trail",
            "spark",
            "smoke",
            "explosion",
            "explode",
            "impact",
            "matcap",
            "flash",
            "glow",
            "beam",
            "blood",
            "splash",
            "dust",
            "debris",
            "flame",
            "shockwave",
            "aura",
            "effect",
        ],
    ),
    (
        Category::Characters,
        &[
            "player",
            "hero",
            "character",
            "skin",
            "body",
            "head",
            "beard",
            "voice",
            "avatar",
            "face",
            "hair",
            "hand",
            "arm",
            "leg",
            "helmet",
            "armor",
            "outfit",
            "costume",
            "npc",
        ],
    ),
    (Category::Music, &["music", "bgm", "soundtrack"]),
];

/// The category an item of `kind` gets when no word decides. Fonts and music are decisive: a
/// font is always part of the HUD, music is always music (only a person or an agent sorting that
/// item moves it; no rule does).
pub fn kind_default(kind: Kind) -> (Category, bool) {
    match kind {
        Kind::Font => (Category::Hud, true),
        Kind::Music => (Category::Music, true),
        Kind::Video => (Category::Hud, false),
        Kind::Map => (Category::Environment, false),
        _ => (Category::Unsorted, false),
    }
}

/// The words of a name or path, lower case: split on anything that isn't a letter or digit, and
/// at camelCase boundaries (`AcidSpitter` → `acid spitter`, `UIButton` → `ui button`).
pub fn words(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut cur = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if !c.is_alphanumeric() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur).to_lowercase());
            }
            continue;
        }
        if c.is_uppercase() && !cur.is_empty() {
            let prev = chars.get(i.wrapping_sub(1)).copied().unwrap_or(' ');
            let next = chars.get(i + 1).copied().unwrap_or(' ');
            // fooBar → foo|Bar; UIButton → UI|Button; HTML5 stays.
            if prev.is_lowercase() || prev.is_ascii_digit() || (prev.is_uppercase() && next.is_lowercase()) {
                out.push(std::mem::take(&mut cur).to_lowercase());
            }
        }
        cur.push(c);
    }
    if !cur.is_empty() {
        out.push(cur.to_lowercase());
    }
    out
}

/// A word matches a keyword: the same, its plural, or (keywords of four letters or more) the
/// word begins with it (`footsteps`, `playerskins`).
fn word_matches(word: &str, key: &str) -> bool {
    word == key
        || word.strip_suffix('s').is_some_and(|w| w == key)
        || word.strip_suffix("es").is_some_and(|w| w == key)
        || word.strip_suffix("ies").is_some_and(|w| key.strip_suffix('y') == Some(w))
        || (key.chars().count() >= 4 && word.starts_with(key))
}

/// A project term matches a list of words: a one-word term like a keyword, several words when
/// they appear together in that order.
fn term_matches(text: &[String], term: &[String]) -> bool {
    match term {
        [] => false,
        [one] => text.iter().any(|w| word_matches(w, one)),
        many => text.windows(many.len()).any(|win| win.iter().zip(many).all(|(w, t)| w == t)),
    }
}

/// The built-in keyword table as a lookup: word → category (first table wins).
fn keyword_map() -> &'static HashMap<&'static str, Category> {
    static MAP: std::sync::OnceLock<HashMap<&'static str, Category>> = std::sync::OnceLock::new();
    MAP.get_or_init(|| {
        let mut m = HashMap::new();
        for (cat, keys) in KEYWORDS {
            for k in *keys {
                m.entry(*k).or_insert(*cat);
            }
        }
        m
    })
}

/// The built-in category of a word that is a keyword or its plural.
fn keyword_exact(word: &str) -> Option<Category> {
    let map = keyword_map();
    if let Some(c) = map.get(word) {
        return Some(*c);
    }
    let singular = [word.strip_suffix("ies").map(|w| format!("{w}y")), word.strip_suffix("es").map(str::to_string), word.strip_suffix('s').map(str::to_string)];
    singular.iter().flatten().find_map(|w| map.get(w.as_str())).copied()
}

/// The built-in category of a word that begins with a keyword of four letters or more (the
/// longest one wins): `footsteps`, `playerskins`.
fn keyword_prefix(word: &str) -> Option<Category> {
    let map = keyword_map();
    (4..word.len()).rev().filter(|l| word.is_char_boundary(*l)).find_map(|l| word.get(..l).and_then(|head| map.get(head)).copied())
}

/// The texts of an item the rules look at, most telling first: its name, its path, its origin.
fn texts(item: &Item) -> [Vec<String>; 3] {
    [words(&item.name), item.path.as_deref().map(words).unwrap_or_default(), item.origin.as_deref().map(words).unwrap_or_default()]
}

/// A project's rules, ready to match.
pub struct Rules {
    rules: Vec<(Vec<String>, Category)>,
}

impl Rules {
    pub fn new(rules: &[CategoryRule]) -> Rules {
        Rules { rules: rules.iter().map(|r| (words(&r.term), r.category)).filter(|(w, _)| !w.is_empty()).collect() }
    }

    /// The category the rules give `item`: the decisive kinds (a song title or a font name never
    /// re-sorts them), the project's terms (name, then path, then origin; the term with the most
    /// words wins, then the first listed), the built-in keywords (same order), then the kind's
    /// default.
    pub fn category(&self, item: &Item) -> Category {
        let (by_kind, decisive) = kind_default(item.kind);
        if decisive {
            return by_kind;
        }
        let texts = texts(item);
        for text in &texts {
            let mut best: Option<(usize, Category)> = None;
            for (term, c) in &self.rules {
                if term_matches(text, term) && best.is_none_or(|(n, _)| term.len() > n) {
                    best = Some((term.len(), *c));
                }
            }
            if let Some((_, c)) = best {
                return c;
            }
        }
        // The name: a whole keyword anywhere in it first, then a word that begins with one. The
        // path and origin (folders and archives, often named for everything in them) only count
        // with whole keywords.
        let [name, path, origin] = &texts;
        name.iter()
            .find_map(|w| keyword_exact(w))
            .or_else(|| name.iter().find_map(|w| keyword_prefix(w)))
            .or_else(|| path.iter().find_map(|w| keyword_exact(w)))
            .or_else(|| origin.iter().find_map(|w| keyword_exact(w)))
            .unwrap_or(by_kind)
    }
}

/// Apply the rules to every item whose category the rules chose (or none was chosen yet); items
/// a person or an agent sorted keep theirs.
pub fn apply(inv: &mut Inventory) {
    let rules = Rules::new(&inv.category_rules);
    for item in inv.items.iter_mut().filter(|i| i.category_by == CategoryBy::Rule) {
        item.category = rules.category(item);
    }
}

/// Words that say nothing about what an asset is for: file and engine jargon.
const NOISE: &[&str] = &[
    "sfx",
    "the",
    "of",
    "and",
    "tex",
    "texture",
    "mat",
    "material",
    "mesh",
    "model",
    "lod",
    "lod0",
    "lod1",
    "lod2",
    "png",
    "wav",
    "ogg",
    "fbx",
    "uasset",
    "default",
    "new",
    "copy",
    "base",
    "normal",
    "diffuse",
    "albedo",
    "mask",
    "masks",
    "rough",
    "roughness",
    "metallic",
    "emissive",
    "small",
    "big",
    "large",
    "medium",
    "left",
    "right",
    "loop",
    "start",
    "stop",
    "end",
    "var",
    "variant",
    "alt",
    "version",
];

/// The most common words among Unsorted items (each item counted once per word), with up to
/// three example names, most common first: what a few project rules would sort.
pub fn unsorted_terms(inv: &Inventory, limit: usize, visible: impl Fn(&Item) -> bool) -> Vec<(String, usize, Vec<String>)> {
    let mut found: BTreeMap<String, (usize, Vec<String>)> = BTreeMap::new();
    for item in inv.items.iter().filter(|i| i.category == Category::Unsorted && visible(i)) {
        let mut seen: Vec<String> = words(&item.name);
        seen.sort();
        seen.dedup();
        for w in seen {
            let hexish = w.len() >= 8 && w.chars().any(|c| c.is_ascii_digit());
            if w.chars().count() < 3 || w.chars().all(|c| c.is_ascii_digit()) || hexish || NOISE.contains(&w.as_str()) {
                continue;
            }
            let e = found.entry(w).or_insert((0, Vec::new()));
            e.0 += 1;
            if e.1.len() < 3 && !e.1.contains(&item.name) {
                e.1.push(item.name.clone());
            }
        }
    }
    let mut v: Vec<(String, usize, Vec<String>)> = found.into_iter().map(|(w, (n, ex))| (w, n, ex)).collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v.truncate(limit);
    v
}

/// A name glob (`*` any run, `?` one character), any case.
pub fn glob_matches(glob: &str, name: &str) -> bool {
    let g: Vec<char> = glob.to_lowercase().chars().collect();
    let n: Vec<char> = name.to_lowercase().chars().collect();
    // Iterative matching with backtracking to the last `*`: linear-ish, never recursive.
    let (mut gi, mut ni, mut star, mut mark) = (0usize, 0usize, None::<usize>, 0usize);
    while ni < n.len() {
        match g.get(gi) {
            Some('*') => {
                star = Some(gi);
                mark = ni;
                gi += 1;
            }
            Some(c) if *c == '?' || Some(c) == n.get(ni) => {
                gi += 1;
                ni += 1;
            }
            _ => match star {
                Some(s) => {
                    gi = s + 1;
                    mark += 1;
                    ni = mark;
                }
                None => return false,
            },
        }
    }
    g.get(gi..).unwrap_or_default().iter().all(|c| *c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::{Source, Status};

    fn item(name: &str, kind: Kind) -> Item {
        Item::new(Source::Original, name, kind, name, Status::Original)
    }

    #[test]
    fn names_split_into_words() {
        assert_eq!(words("AcidSpitter_Body-LOD0"), ["acid", "spitter", "body", "lod0"]);
        assert_eq!(words("UIButton hover"), ["ui", "button", "hover"]);
        assert_eq!(words("sfx_bugs_small DRAK 25"), ["sfx", "bugs", "small", "drak", "25"]);
        assert_eq!(words("PS4 XboxSeriesX"), ["ps4", "xbox", "series", "x"]);
        assert_eq!(words("C4Pallet_MAT_Base"), ["c4", "pallet", "mat", "base"]);
        assert!(words("—").is_empty());
    }

    #[test]
    fn keywords_and_kinds_sort_items() {
        let r = Rules::new(&[]);
        assert_eq!(r.category(&item("Icons_CM_Back", Kind::Texture)), Category::Hud);
        assert_eq!(r.category(&item("sfx_bugs_small_death", Kind::Sound)), Category::Enemies);
        assert_eq!(r.category(&item("Weapon_Reload_01", Kind::Sound)), Category::Weapons);
        assert_eq!(r.category(&item("amb_cave_wind", Kind::Sound)), Category::Environment);
        assert_eq!(r.category(&item("footsteps_dirt", Kind::Sound)), Category::Environment);
        assert_eq!(r.category(&item("Trail_Matcap", Kind::Texture)), Category::Effects);
        assert_eq!(r.category(&item("Scout_BEARD", Kind::Model)), Category::Characters);
        assert_eq!(r.category(&item("Turret_Base", Kind::Model)), Category::Machines);
        assert_eq!(r.category(&item("Gold_Chunk", Kind::Model)), Category::Items);
        assert_eq!(r.category(&item("playerskins", Kind::Texture)), Category::Characters, "begins with a keyword");
        // Fonts and music are decisive over keywords; other kinds only fill in.
        assert_eq!(r.category(&item("BossFont", Kind::Font)), Category::Hud);
        assert_eq!(r.category(&item("Boss_Theme", Kind::Music)), Category::Music);
        assert_eq!(r.category(&item("intro", Kind::Video)), Category::Hud);
        assert_eq!(r.category(&item("Glyphid_Grunt", Kind::Texture)), Category::Unsorted);
        // The path and origin count when the name says nothing.
        let mut i = item("T_0042", Kind::Texture);
        i.path = Some("app/Content/UI/T_0042.png".into());
        assert_eq!(r.category(&i), Category::Hud);
    }

    #[test]
    fn project_terms_win_over_keywords_and_people_over_rules() {
        let mut inv = Inventory {
            items: vec![item("Glyphid_Grunt", Kind::Texture), item("sfx_DRAK_25_shot", Kind::Sound), item("Scout_Head", Kind::Model)],
            category_rules: vec![
                CategoryRule { term: "Glyphid".into(), category: Category::Enemies },
                CategoryRule { term: "drak 25".into(), category: Category::Weapons },
                CategoryRule { term: "scout".into(), category: Category::Characters },
            ],
            ..Default::default()
        };
        apply(&mut inv);
        let cats: Vec<Category> = inv.items.iter().map(|i| i.category).collect();
        assert_eq!(cats, [Category::Enemies, Category::Weapons, Category::Characters]);
        // The term with more words wins whatever the order: "scout head" over "scout".
        inv.category_rules.push(CategoryRule { term: "scout head".into(), category: Category::Enemies });
        apply(&mut inv);
        assert_eq!(inv.items[2].category, Category::Enemies);
        // No rule moves music or fonts: a song named for an enemy is still music.
        inv.items.push(item("02 Attack of the Glyphids", Kind::Music));
        apply(&mut inv);
        assert_eq!(inv.items[3].category, Category::Music);
        // People and agents keep theirs when the rules are applied again.
        inv.items[0].category = Category::Hud;
        inv.items[0].category_by = CategoryBy::User;
        inv.items[1].category = Category::Effects;
        inv.items[1].category_by = CategoryBy::Agent;
        apply(&mut inv);
        assert_eq!((inv.items[0].category, inv.items[1].category), (Category::Hud, Category::Effects));
    }

    #[test]
    fn unsorted_terms_count_items_and_skip_noise() {
        let mut inv = Inventory {
            items: vec![item("Glyphid_Grunt", Kind::Texture), item("Glyphid_Praetorian", Kind::Texture), item("T_mat_9f3a2b1c", Kind::Texture)],
            ..Default::default()
        };
        apply(&mut inv);
        let t = unsorted_terms(&inv, 10, |_| true);
        assert_eq!(t[0].0, "glyphid");
        assert_eq!(t[0].1, 2);
        assert_eq!(t[0].2, ["Glyphid_Grunt", "Glyphid_Praetorian"]);
        assert!(!t.iter().any(|(w, ..)| w == "mat" || w == "9f3a2b1c"));
    }

    #[test]
    fn globs_match_names() {
        assert!(glob_matches("glyphid*", "Glyphid_Grunt"));
        assert!(glob_matches("*_body", "Scout_BODY"));
        assert!(glob_matches("sfx_?ugs*", "sfx_bugs_small"));
        assert!(!glob_matches("glyphid", "Glyphid_Grunt"));
        assert!(glob_matches("*", ""));
        assert!(!glob_matches("a*b*c", "aXbXd"));
    }
}
