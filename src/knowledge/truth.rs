//! The compendium's World mode: current world truth and the hidden world
//! bible, in the same categories as the Player-mode entries.
//!
//! Everything here is read straight from stored state (entities, rules,
//! the glossary and the bible entity) and none of it from learned entries,
//! so it shows what is so rather than what the player knows. It writes
//! nothing and generates no prose: a summary is a stored description or
//! nothing. Each entry says where it comes from: an instantiated world
//! entity or rule, or a bible plan or reference that may never have been
//! instantiated.

use std::fmt::Write;

use serde_json::Value;

use super::Category;
use crate::game;
use crate::glossary;
use crate::inspect::{clip, clip_value, rule_text};
use crate::rules;
use crate::world::{
    ENGINE_PROP_PREFIX, Entity, EntityId, PLAYER_KIND, RESERVED_KINDS, ROOM_KIND, StoreError,
    World, is_entry,
};

/// Which compendium view: what the character has learned, or the world as
/// it is. A view choice only; neither reads nor writes the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Player,
    World,
}

impl Mode {
    /// The mode a word names (`player`, `world`, any case).
    pub fn parse(word: &str) -> Option<Mode> {
        match word.to_lowercase().as_str() {
            "player" => Some(Mode::Player),
            "world" => Some(Mode::World),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Mode::Player => "Player",
            Mode::World => "World",
        }
    }

    /// The other mode.
    pub fn toggled(self) -> Mode {
        match self {
            Mode::Player => Mode::World,
            Mode::World => Mode::Player,
        }
    }
}

/// Splits a leading `player` or `world` off a compendium argument. The
/// word names a mode only before nothing, a category, `search …` or an
/// exact key (`#12`, `bible.npcs.2`, `glossary.lit`): otherwise it is the
/// start of a name (`World Tree`). So what is left never starts with a
/// mode word itself, and splitting it again changes nothing.
pub fn split_mode(arg: &str) -> (Option<Mode>, &str) {
    let arg = arg.trim();
    let (word, rest) = arg.split_once(char::is_whitespace).unwrap_or((arg, ""));
    let rest = rest.trim();
    let first = rest.split_whitespace().next().unwrap_or("").to_lowercase();
    let selects = rest.is_empty()
        || (first == rest.to_lowercase() && Category::parse(rest).is_some())
        || first == "search"
        || (first == rest.to_lowercase()
            && (rest.starts_with('#')
                || first.starts_with("bible.")
                || first.starts_with("glossary.")));
    match Mode::parse(word) {
        Some(mode) if selects => (Some(mode), rest),
        _ => (None, arg),
    }
}

/// Where a World-mode entry comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// An instantiated world entity or rule; `in_bible` when the bible
    /// also plans it (by name).
    World { in_bible: bool },
    /// A retired rule: still stored, no longer applied.
    Retired,
    /// A room stub: an exit's far end, stored but not yet generated.
    Unexplored,
    /// A bible plan (an NPC, region or faction) with no world entity.
    BiblePlan,
    /// Hidden bible reference (the tone, a secret): background truth that
    /// is never an entity.
    BibleReference,
}

impl Origin {
    pub fn label(self) -> &'static str {
        match self {
            Origin::World { in_bible: false } => "world",
            Origin::World { in_bible: true } => "world, in bible",
            Origin::Retired => "world, retired",
            Origin::Unexplored => "world, unexplored",
            Origin::BiblePlan => "bible plan",
            Origin::BibleReference => "bible reference",
        }
    }

    /// Bible material rather than world state.
    pub fn is_bible(self) -> bool {
        matches!(self, Origin::BiblePlan | Origin::BibleReference)
    }
}

/// One World-mode entry.
#[derive(Debug, Clone, PartialEq)]
pub struct WorldEntry {
    /// How to name it exactly: `#12` for an entity or rule,
    /// `bible.npcs.2` for a bible item, `glossary.<term>` for a term.
    pub key: String,
    pub name: String,
    pub category: Category,
    pub origin: Origin,
    /// A stored description, or empty.
    pub summary: String,
    /// The rest of what is stored, a line each.
    pub detail: Vec<String>,
}

impl WorldEntry {
    fn mentions(&self, needle: &str) -> bool {
        self.name.to_lowercase().contains(needle)
            || self.summary.to_lowercase().contains(needle)
            || self
                .detail
                .iter()
                .any(|d| d.to_lowercase().contains(needle))
    }
}

/// One item of a bible section.
struct BibleItem {
    key: String,
    name: String,
    summary: String,
    detail: Vec<String>,
}

fn bible(world: &World) -> Result<Option<Entity>, StoreError> {
    Ok(match game::bible_id(world)? {
        Some(id) => world.entity(id)?,
        None => None,
    })
}

/// The items of bible section `section`: an object with a `name` (and a
/// `description`, and anything else as detail lines), or a bare string.
fn bible_items(bible: Option<&Entity>, section: &str) -> Vec<BibleItem> {
    let items = match bible.and_then(|b| b.props.get(section)) {
        Some(Value::Array(items)) => items.clone(),
        Some(Value::Null) | None => Vec::new(),
        Some(one) => vec![one.clone()],
    };
    let mut out = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let key = format!("bible.{section}.{}", i + 1);
        let (name, summary, detail) = match item {
            // A bare name (an NPC, a region) whole, so it matches its
            // entity; a sentence (a secret) cut to a title.
            Value::String(s)
                if section == "npcs" || section == "regions" || section == "factions" =>
            {
                (s.trim().to_owned(), String::new(), Vec::new())
            }
            Value::String(s) => (clip(s, 60), s.trim().to_owned(), Vec::new()),
            Value::Object(map) => {
                let name = map
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|n| !n.is_empty())
                    .unwrap_or("(unnamed)")
                    .to_owned();
                let summary = map
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim()
                    .to_owned();
                let detail = map
                    .iter()
                    .filter(|(k, _)| *k != "name" && *k != "description")
                    .map(|(k, v)| format!("{k}: {}", text_of(v)))
                    .collect();
                (name, summary, detail)
            }
            other => (format!("item {}", i + 1), text_of(other), Vec::new()),
        };
        out.push(BibleItem {
            key,
            name,
            summary,
            detail,
        });
    }
    out
}

fn bible_entry(item: BibleItem, category: Category, origin: Origin) -> WorldEntry {
    WorldEntry {
        key: item.key,
        name: item.name,
        category,
        origin,
        summary: item.summary,
        detail: item.detail,
    }
}

/// A string as itself, anything else as clipped JSON.
fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.trim().to_owned(),
        other => clip_value(other),
    }
}

fn same_name(a: &str, b: &str) -> bool {
    a.trim().to_lowercase() == b.trim().to_lowercase()
}

/// Every entity, walking the containment tree from the top level.
fn all_entities(world: &World) -> Result<Vec<Entity>, StoreError> {
    let mut out = Vec::new();
    let mut todo = world.contents(None)?;
    while let Some(id) = todo.pop() {
        if let Some(e) = world.entity(id)? {
            todo.extend(world.contents(Some(id))?);
            out.push(e);
        }
    }
    out.sort_by_key(|e| e.id);
    Ok(out)
}

/// An entity as a World-mode entry: its description as the summary, then
/// its kind, where it is, its other props and its contents.
fn entity_entry(
    world: &World,
    e: &Entity,
    category: Category,
    in_bible: Option<&BibleItem>,
) -> Result<WorldEntry, StoreError> {
    let summary = e.props.get("description").map(text_of).unwrap_or_default();
    let mut detail = vec![format!("kind: {}", e.kind)];
    if e.kind != ROOM_KIND {
        detail.push(format!("where: {}", place_of(world, e)?));
    }
    for (k, v) in &e.props {
        if k == "description" || k.starts_with(ENGINE_PROP_PREFIX) {
            continue;
        }
        detail.push(format!("{k} = {}", clip_value(v)));
    }
    let inside: Vec<String> = world
        .contents(Some(e.id))?
        .into_iter()
        .filter_map(|id| world.entity(id).transpose())
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|c| !is_entry(c))
        .map(|c| format!("#{} {}", c.id, c.name))
        .collect();
    if !inside.is_empty() {
        detail.push(format!("holds: {}", inside.join(", ")));
    }
    if let Some(b) = in_bible {
        if !b.summary.is_empty() {
            detail.push(format!("bible ({}): {}", b.key, b.summary));
        }
        detail.extend(b.detail.iter().map(|d| format!("bible: {d}")));
    }
    Ok(WorldEntry {
        key: format!("#{}", e.id),
        name: e.name.clone(),
        category,
        origin: Origin::World {
            in_bible: in_bible.is_some(),
        },
        summary,
        detail,
    })
}

/// Where `e` is: the chain of what holds it, innermost first.
fn place_of(world: &World, e: &Entity) -> Result<String, StoreError> {
    let mut chain = Vec::new();
    let mut seen = vec![e.id];
    let mut at = e.location;
    while let Some(id) = at {
        // An older save's containment cycle ends the walk, not the read.
        if seen.contains(&id) {
            break;
        }
        seen.push(id);
        let Some(holder) = world.entity(id)? else {
            break;
        };
        chain.push(format!("#{} {}", holder.id, holder.name));
        at = holder.location;
    }
    Ok(if chain.is_empty() {
        "nowhere (top level)".to_owned()
    } else {
        format!("in {}", chain.join(", in "))
    })
}

/// Every World-mode entry, by category, instantiated state before bible
/// plans within each.
pub fn world_entries(world: &World) -> Result<Vec<WorldEntry>, StoreError> {
    let bible = bible(world)?;
    let bible = bible.as_ref();
    let mut out = Vec::new();

    // People: NPC entities, then the bible's NPCs with no entity.
    let mut npc_plans = bible_items(bible, "npcs");
    let npcs = game::npcs(world)?;
    let mut people = Vec::new();
    for s in &npcs {
        if is_entry(&s.npc) {
            continue;
        }
        let planned = npc_plans
            .iter()
            .position(|b| same_name(&b.name, &s.npc.name));
        let planned = planned.map(|i| npc_plans.remove(i));
        people.push(entity_entry(
            world,
            &s.npc,
            Category::Person,
            planned.as_ref(),
        )?);
    }
    out.extend(people);
    out.extend(
        npc_plans
            .into_iter()
            .map(|b| bible_entry(b, Category::Person, Origin::BiblePlan)),
    );

    // Places: rooms that exist (a region of the same name is their bible
    // plan), then the bible's other regions.
    let everything = all_entities(world)?;
    let mut region_plans = bible_items(bible, "regions");
    for e in &everything {
        if e.kind != ROOM_KIND {
            continue;
        }
        if game::is_stub_entity(e)? {
            let mut stub = entity_entry(world, e, Category::Place, None)?;
            stub.origin = Origin::Unexplored;
            out.push(stub);
            continue;
        }
        let planned = region_plans
            .iter()
            .position(|b| same_name(&b.name, &e.name));
        let planned = planned.map(|i| region_plans.remove(i));
        out.push(entity_entry(world, e, Category::Place, planned.as_ref())?);
    }
    out.extend(
        region_plans
            .into_iter()
            .map(|b| bible_entry(b, Category::Place, Origin::BiblePlan)),
    );

    // Factions live only in the bible.
    out.extend(
        bible_items(bible, "factions")
            .into_iter()
            .map(|b| bible_entry(b, Category::Faction, Origin::BiblePlan)),
    );

    // Items: every other ordinary entity.
    let acts: Vec<EntityId> = npcs.iter().map(|s| s.npc.id).collect();
    for e in &everything {
        let engine = RESERVED_KINDS.contains(&e.kind.as_str()) || e.kind == PLAYER_KIND;
        if engine || is_entry(e) || acts.contains(&e.id) {
            continue;
        }
        out.push(entity_entry(world, e, Category::Item, None)?);
    }

    // Lore: the bible's tone and secrets.
    if let Some(tone) = bible.and_then(|b| b.props.get("tone")) {
        out.push(WorldEntry {
            key: "bible.tone".into(),
            name: "Tone".into(),
            category: Category::Lore,
            origin: Origin::BibleReference,
            summary: text_of(tone),
            detail: Vec::new(),
        });
    }
    out.extend(
        bible_items(bible, "secrets")
            .into_iter()
            .map(|b| bible_entry(b, Category::Lore, Origin::BibleReference)),
    );

    // Mechanics: every rule as stored, then the glossary.
    for r in rules::all(world)? {
        let text = rule_text(&r);
        // Past the heading, shown as the entry's own.
        let lines = text.lines().skip(1).map(str::trim).map(str::to_owned);
        out.push(WorldEntry {
            key: format!("#{}", r.id),
            name: r.def.name.clone(),
            category: Category::Mechanic,
            origin: if r.is_active() {
                Origin::World { in_bible: false }
            } else {
                Origin::Retired
            },
            summary: format!(
                "{} rule, version {}",
                match r.class {
                    rules::RuleClass::World => "World",
                    rules::RuleClass::Stat => "Stat",
                },
                r.version
            ),
            detail: lines.collect(),
        });
    }
    for (name, term) in glossary::terms(world)? {
        let mut detail: Vec<String> = term
            .values
            .iter()
            .map(|(v, m)| match m.trim() {
                "" => format!("= {v}"),
                m => format!("= {v}: {m}"),
            })
            .collect();
        if term.free_text {
            detail.insert(0, "free text".into());
        }
        out.push(WorldEntry {
            key: format!("glossary.{name}"),
            summary: match term.meaning.trim() {
                "" => "Glossary term, undefined.".into(),
                m => format!("Glossary term: {m}"),
            },
            name,
            category: Category::Mechanic,
            origin: Origin::World { in_bible: false },
            detail,
        });
    }
    Ok(out)
}

/// What a name or key finds among World-mode entries.
#[derive(Debug, Clone, PartialEq)]
pub enum WorldFound {
    None,
    One(WorldEntry),
    Many(Vec<WorldEntry>),
}

/// Find a World-mode entry by key (`#12`, `bible.npcs.2`), exact name or a
/// unique part of a name (any case).
pub fn world_find(world: &World, query: &str) -> Result<WorldFound, StoreError> {
    let q = query.trim().to_lowercase();
    let all = world_entries(world)?;
    let pick = |mut matches: Vec<WorldEntry>| match matches.len() {
        0 => None,
        1 => Some(WorldFound::One(matches.remove(0))),
        _ => Some(WorldFound::Many(matches)),
    };
    let (keyed, all): (Vec<_>, Vec<_>) = all.into_iter().partition(|e| e.key.to_lowercase() == q);
    if let Some(f) = pick(keyed) {
        return Ok(f);
    }
    let (exact, rest): (Vec<_>, Vec<_>) = all.into_iter().partition(|e| e.name.to_lowercase() == q);
    if let Some(f) = pick(exact) {
        return Ok(f);
    }
    let partial = rest
        .into_iter()
        .filter(|e| e.name.to_lowercase().contains(&q))
        .collect();
    Ok(pick(partial).unwrap_or(WorldFound::None))
}

/// World-mode entries whose name, summary or detail contain `text`.
pub fn world_search(world: &World, text: &str) -> Result<Vec<WorldEntry>, StoreError> {
    let needle = text.trim().to_lowercase();
    Ok(world_entries(world)?
        .into_iter()
        .filter(|e| e.mentions(&needle))
        .collect())
}

const WORLD_HINT: &str = ":compendium world <name|#id|bible.<section>.<n>> shows an entry; \
:compendium world search <text> searches; :mode player goes back to what you know.";

/// `:compendium world [<category> | <name|key> | search <text>]`.
pub(super) fn world_report(world: &World, arg: &str) -> Result<String, StoreError> {
    let arg = arg.trim();
    let (word, rest) = arg.split_once(char::is_whitespace).unwrap_or((arg, ""));
    if arg.is_empty() {
        return world_overview(world);
    }
    if word.eq_ignore_ascii_case("search") {
        let text = rest.trim();
        if text.is_empty() {
            return Ok(super::report::USAGE.into());
        }
        let hits = world_search(world, text)?;
        if hits.is_empty() {
            return Ok(format!(
                "Nothing in the world or the bible mentions {text:?}."
            ));
        }
        let mut out = format!("World mode: entries mentioning {text:?}:");
        for e in &hits {
            write!(out, "\n  {}", world_line(e)).unwrap();
        }
        return Ok(out);
    }
    if rest.is_empty()
        && let Some(category) = Category::parse(word)
    {
        let all: Vec<WorldEntry> = world_entries(world)?
            .into_iter()
            .filter(|e| e.category == category)
            .collect();
        if all.is_empty() {
            return Ok(format!(
                "World mode: no {} in the world or the bible.",
                category.heading().to_lowercase()
            ));
        }
        let mut out = format!("{} · World mode\n", category.heading());
        for e in &all {
            writeln!(out, "  {}", world_line(e)).unwrap();
        }
        write!(out, "\n{WORLD_HINT}").unwrap();
        return Ok(out);
    }
    Ok(match world_find(world, arg)? {
        WorldFound::One(e) => world_detail(&e),
        WorldFound::None => format!(
            "Nothing in the world or the bible is called {arg:?}.\n{}",
            super::report::USAGE
        ),
        WorldFound::Many(many) => {
            let mut out = format!("{arg:?} could be several entries; use a key:");
            for e in &many {
                write!(out, "\n  {}", world_line(e)).unwrap();
            }
            out
        }
    })
}

fn world_overview(world: &World) -> Result<String, StoreError> {
    let all = world_entries(world)?;
    let mut out =
        String::from("Compendium · World mode: the world as it is, and the hidden bible\n");
    for c in Category::ALL {
        let of: Vec<&WorldEntry> = all.iter().filter(|e| e.category == c).collect();
        let plans = of.iter().filter(|e| e.origin.is_bible()).count();
        let real = of.len() - plans;
        writeln!(
            out,
            "  {} ({real} in the world, {plans} in the bible only)",
            c.heading()
        )
        .unwrap();
    }
    for c in Category::ALL {
        let of: Vec<&WorldEntry> = all.iter().filter(|e| e.category == c).collect();
        if of.is_empty() {
            continue;
        }
        writeln!(out, "\n{}", c.heading()).unwrap();
        for e in of {
            writeln!(out, "  {}", world_line(e)).unwrap();
        }
    }
    write!(out, "\n{WORLD_HINT}").unwrap();
    Ok(out)
}

/// `#12 Reeve Marrick (person · world, in bible)`.
fn world_line(e: &WorldEntry) -> String {
    format!(
        "{} {} ({} · {})",
        e.key,
        e.name,
        e.category.label(),
        e.origin.label()
    )
}

fn world_detail(e: &WorldEntry) -> String {
    let mut out = format!(
        "{} · {} · {} · {} · World mode\n",
        e.name,
        e.category.label(),
        e.origin.label(),
        e.key
    );
    if e.summary.is_empty() {
        out.push_str("(no stored description)\n");
    } else {
        writeln!(out, "{}", e.summary).unwrap();
    }
    if !e.detail.is_empty() {
        out.push('\n');
        for d in &e.detail {
            writeln!(out, "- {d}").unwrap();
        }
    }
    let note = match e.origin {
        Origin::BiblePlan => {
            "\nA bible plan, not instantiated: nothing in the world has this name yet."
        }
        Origin::BibleReference => {
            "\nHidden bible reference: background truth the director works from, not a world \
             entity."
        }
        _ => "\nStored world state, as it is now; the character may know none of it.",
    };
    out.push_str(note);
    out
}
