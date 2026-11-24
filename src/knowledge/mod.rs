//! What the player has learned: the compendium's Player-mode knowledge.
//!
//! Knowledge is kept apart from world truth (see the decision "Loam
//! separates learned knowledge from world truth"). Each compendium entry is
//! its own top-level entity of kind [`KNOWLEDGE_KIND`], so its id is a
//! stable entry identity independent of the world entity it describes.
//! Its state is the engine-owned `_entry` component, changed only through
//! engine patches, so the event log (and so replay, forks and adopted
//! rerolls) carries it like any other state. The world store seals any
//! entity carrying `_entry`: director patches can't change, rename, move,
//! relate or destroy an entry.
//!
//! An entry stores what was learned when it was learned: readers never look
//! at the world entity it was about, so later changes off-screen, or its
//! destruction, leave the remembered account as it was.
//!
//! [`record_place`] is the one writer so far; every write cites a
//! player-visible source event (a non-empty `narration`), checked before
//! anything is written. The queries read entries only, so nothing the
//! player hasn't learned can show up in a list, a name match or a search.

mod report;

#[cfg(test)]
mod tests;

pub use report::compendium;

use serde::{Deserialize, Serialize};

use crate::world::{
    self, ApplyError, Component, EXIT_REL, EntityId, EventKind, KNOWLEDGE_KIND, Op, Patch,
    ROOM_KIND, StoreError, World,
};

/// The compendium's sections.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Person,
    Place,
    Faction,
    Item,
    Lore,
    Mechanic,
}

impl Category {
    pub const ALL: [Category; 6] = [
        Category::Person,
        Category::Place,
        Category::Faction,
        Category::Item,
        Category::Lore,
        Category::Mechanic,
    ];

    /// The section's heading, plural.
    pub fn heading(self) -> &'static str {
        match self {
            Category::Person => "People",
            Category::Place => "Places",
            Category::Faction => "Factions",
            Category::Item => "Items",
            Category::Lore => "Lore",
            Category::Mechanic => "Mechanics",
        }
    }

    /// One entry's label, singular.
    pub fn label(self) -> &'static str {
        match self {
            Category::Person => "person",
            Category::Place => "place",
            Category::Faction => "faction",
            Category::Item => "item",
            Category::Lore => "lore",
            Category::Mechanic => "mechanic",
        }
    }

    /// The category a word names (`places`, `place`, `people`…).
    pub fn parse(word: &str) -> Option<Category> {
        let w = word.to_lowercase();
        Category::ALL
            .into_iter()
            .find(|c| w == c.label() || w == c.heading().to_lowercase())
    }
}

/// How a fact was learned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactKind {
    /// The player saw it for themselves.
    Observed,
}

/// One thing learned about an entry, with the events that support it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fact {
    pub text: String,
    pub kind: FactKind,
    /// Player-visible events supporting it, oldest first.
    pub sources: Vec<i64>,
}

/// An entry's state: the `_entry` component.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    category: Category,
    /// The world entity this entry was learned about, when there is one.
    /// Kept only to find the entry again on a later encounter: entity ids
    /// are never reused, and nothing reads the entity to show the entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subject: Option<EntityId>,
    /// The last-known account, as last seen.
    summary: String,
    /// The event the summary was taken from.
    summary_source: i64,
    first_seen: i64,
    last_seen: i64,
    /// How many separate encounters the entry has had.
    encounters: u32,
    #[serde(default)]
    facts: Vec<Fact>,
}

impl Component for Record {
    const KEY: &'static str = world::ENTRY_PROP;
}

/// A compendium entry as the player knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The entry's own stable id (not the id of what it describes).
    pub id: EntityId,
    pub name: String,
    pub category: Category,
    /// The last-known account.
    pub summary: String,
    pub summary_source: i64,
    /// The first and latest events that recorded an encounter.
    pub first_seen: i64,
    pub last_seen: i64,
    pub encounters: u32,
    pub facts: Vec<Fact>,
}

impl Entry {
    fn new(id: EntityId, name: String, r: Record) -> Entry {
        Entry {
            id,
            name,
            category: r.category,
            summary: r.summary,
            summary_source: r.summary_source,
            first_seen: r.first_seen,
            last_seen: r.last_seen,
            encounters: r.encounters,
            facts: r.facts,
        }
    }

    /// Whether `needle` (lowercase) occurs in the entry's name, summary or
    /// facts.
    fn mentions(&self, needle: &str) -> bool {
        self.name.to_lowercase().contains(needle)
            || self.summary.to_lowercase().contains(needle)
            || self
                .facts
                .iter()
                .any(|f| f.text.to_lowercase().contains(needle))
    }
}

/// Every entry, in the order first learned.
pub fn entries(world: &World) -> Result<Vec<Entry>, StoreError> {
    world
        .query::<Record>()?
        .into_iter()
        .map(|(id, r)| load(world, id, r))
        .collect()
}

/// One entry by its id, if `id` is an entry.
pub fn entry(world: &World, id: EntityId) -> Result<Option<Entry>, StoreError> {
    world
        .get::<Record>(id)?
        .map(|r| load(world, id, r))
        .transpose()
}

fn load(world: &World, id: EntityId, r: Record) -> Result<Entry, StoreError> {
    // The entry's own entity, sealed from the director: never the subject.
    let name = world
        .entity(id)?
        .ok_or_else(|| StoreError::Corrupt(format!("compendium entry #{id} vanished")))?
        .name;
    Ok(Entry::new(id, name, r))
}

/// Entries whose name, summary or facts contain `text` (any case).
pub fn search(world: &World, text: &str) -> Result<Vec<Entry>, StoreError> {
    let needle = text.trim().to_lowercase();
    Ok(entries(world)?
        .into_iter()
        .filter(|e| e.mentions(&needle))
        .collect())
}

/// What a name or `#id` finds among known entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    None,
    One(Entry),
    Many(Vec<Entry>),
}

/// Find an entry by `#id`, exact name, or a unique part of a name (any
/// case). Only entries are searched.
pub fn find(world: &World, query: &str) -> Result<Found, StoreError> {
    let query = query.trim();
    let all = entries(world)?;
    if let Some(Ok(id)) = query.strip_prefix('#').map(str::parse::<EntityId>) {
        return Ok(all
            .into_iter()
            .find(|e| e.id == id)
            .map_or(Found::None, Found::One));
    }
    let q = query.to_lowercase();
    let pick = |mut matches: Vec<Entry>| match matches.len() {
        0 => None,
        1 => Some(Found::One(matches.remove(0))),
        _ => Some(Found::Many(matches)),
    };
    let (exact, rest): (Vec<_>, Vec<_>) = all.into_iter().partition(|e| e.name.to_lowercase() == q);
    if let Some(f) = pick(exact) {
        return Ok(f);
    }
    let partial = rest
        .into_iter()
        .filter(|e| e.name.to_lowercase().contains(&q))
        .collect();
    Ok(pick(partial).unwrap_or(Found::None))
}

/// The entry learned about world entity `subject` in `category`, if any.
fn about(
    world: &World,
    category: Category,
    subject: EntityId,
) -> Result<Option<(EntityId, Record)>, StoreError> {
    Ok(world
        .query::<Record>()?
        .into_iter()
        .find(|(_, r)| r.category == category && r.subject == Some(subject)))
}

/// Check that `seq` is player-visible evidence: an event in the log that
/// is narration the player was shown. `Err` says why not.
fn check_source(world: &World, seq: i64) -> Result<Result<(), String>, StoreError> {
    let Some(event) = world.event(seq)? else {
        return Ok(Err(format!("source #{seq} is not in the log")));
    };
    if event.kind != EventKind::Narration {
        return Ok(Err(format!(
            "source #{seq} is a {} event, not narration the player saw",
            event.kind.as_str()
        )));
    }
    if event.payload["text"]
        .as_str()
        .is_none_or(|t| t.trim().is_empty())
    {
        return Ok(Err(format!(
            "source #{seq} narrates nothing, so it supports nothing"
        )));
    }
    Ok(Ok(()))
}

/// Remember that the player entered room `room`, as narrated by `source`
/// (a narration event), coming from room `from` when they came from one.
///
/// Records only what the encounter justifies: the room's name, the
/// description they found there, and the known place they reached it from
/// (when an exit joins the two directly).
/// No other prop, exit or content is copied. Re-entering updates the same
/// entry, so a place keeps one identity. Returns the entry's id, or `Err`
/// with why nothing was written (an invalid source, or `room` is not a
/// room).
pub(crate) fn record_place(
    world: &mut World,
    room: EntityId,
    from: Option<EntityId>,
    source: i64,
) -> Result<Result<EntityId, String>, StoreError> {
    if let Err(why) = check_source(world, source)? {
        return Ok(Err(why));
    }
    let Some(place) = world.entity(room)?.filter(|e| e.kind == ROOM_KIND) else {
        return Ok(Err(format!("#{room} is not a room")));
    };
    let description = place
        .props
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim()
        .to_owned();
    // Named as the player knows the place they came from, not as it is now;
    // only when an exit leads straight here, so a turn that passed through
    // other rooms on the way claims no false neighbour.
    let adjacent = match from.filter(|&f| f != room) {
        Some(f) => world
            .relations_from(f, Some(EXIT_REL))?
            .iter()
            .any(|r| r.object == room)
            .then_some(f),
        None => None,
    };
    let came_from = match adjacent {
        Some(f) => match about(world, Category::Place, f)? {
            Some((id, _)) => entry(world, id)?.map(|e| e.name),
            None => None,
        },
        None => None,
    };

    let (ops, created) = match about(world, Category::Place, room)? {
        Some((id, mut r)) => {
            r.summary = description;
            r.summary_source = source;
            r.last_seen = source;
            r.encounters += 1;
            if let Some(name) = came_from {
                add_fact(&mut r.facts, format!("Reached from {name}."), source);
            }
            let mut ops = vec![world::set_op(id, &r)];
            if world.entity(id)?.is_some_and(|e| e.name != place.name) {
                ops.push(Op::Rename {
                    entity: id.into(),
                    name: place.name.clone(),
                });
            }
            (ops, Some(id))
        }
        None => {
            let mut facts = Vec::new();
            if let Some(name) = came_from {
                add_fact(&mut facts, format!("Reached from {name}."), source);
            }
            let record = Record {
                category: Category::Place,
                subject: Some(room),
                summary: description,
                summary_source: source,
                first_seen: source,
                last_seen: source,
                encounters: 1,
                facts,
            };
            let op = Op::Create {
                handle: None,
                name: place.name.clone(),
                kind: KNOWLEDGE_KIND.into(),
                location: None,
                props: [world::prop(&record)].into_iter().collect(),
            };
            (vec![op], None)
        }
    };
    match world.apply_engine_patch(&Patch::new(ops)) {
        Ok(applied) => Ok(Ok(created.unwrap_or_else(|| applied.created[0]))),
        Err(ApplyError::Rejected(r)) => Ok(Err(format!("patch rejected: {r}"))),
        Err(ApplyError::Storage(e)) => Err(StoreError::Sqlite(e)),
    }
}

/// Add an observed fact, or cite `source` for it again if already known.
fn add_fact(facts: &mut Vec<Fact>, text: String, source: i64) {
    match facts.iter_mut().find(|f| f.text == text) {
        Some(f) => {
            if !f.sources.contains(&source) {
                f.sources.push(source);
            }
        }
        None => facts.push(Fact {
            text,
            kind: FactKind::Observed,
            sources: vec![source],
        }),
    }
}
