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
//! Three writers: [`record_place`] for rooms the player enters,
//! [`record_encounters`] for the people and notable items a turn's
//! narration names around the player, and [`learn`] for discoveries
//! (observations and attributed claims about any category, including
//! subjects only mentioned, which need no world entity; explicit links of
//! such an entry to the entity it turned out to be; and corrections, which
//! supersede earlier facts without erasing them). The
//! director submits discoveries with the `learn` tool; they are staged
//! until the turn's narration exists, then committed ([`tools`]). Every
//! write cites player-visible source events (non-empty `narration`, and for
//! a discovery the excerpt it quotes), checked before anything is written.
//! The queries read entries only, so nothing the player hasn't learned can
//! show up in a list, a name match or a search.
//!
//! A mechanic may be linked to the rule or glossary term behind it
//! ([`Link`]). Its facts record the rule's version when learned, and
//! [`record_use`] counts fast-path uses of a learned rule, but nothing ever
//! reads the rule or term to show an entry: amending or retiring the rule
//! leaves what was learned as it was, until a later narrated fact
//! supersedes it.

mod report;
pub(crate) mod tools;

#[cfg(test)]
mod tests;

pub use report::{compendium, learned_rules, learned_terms};

use serde::{Deserialize, Serialize};

use crate::glossary;
use crate::rules::{self, RuleId};
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
    /// Someone told the player (or it is said): attributed, never
    /// established truth.
    Claimed,
}

/// One thing learned about an entry, with the events that support it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fact {
    pub text: String,
    pub kind: FactKind,
    /// Who made a claim, as narrated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_by: Option<String>,
    /// The entry for whoever made the claim, when one was named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimant: Option<EntityId>,
    /// Player-visible events supporting it, oldest first.
    pub sources: Vec<Source>,
    /// Earlier facts of the same entry this one corrects, by number
    /// (1-based, in the order learned). They stay, with their own sources
    /// and attribution, but no longer shape the summary.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supersedes: Vec<usize>,
    /// The version of the entry's linked rule when this was learned: what
    /// was learned stays tied to the rule as it was then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule_version: Option<u32>,
}

/// What a mechanic entry is about in the engine: a rule, or a glossary
/// term. Player views never read the rule or term itself, so a link reveals
/// none of its conditions, effects, roles or meanings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Link {
    Rule(RuleId),
    Term(String),
}

/// How often the player has used a learned mechanic (a rule applied to
/// their action on the fast path), and when.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub count: u32,
    /// The narration of the first and latest uses.
    pub first: i64,
    pub last: i64,
    /// The linked rule's version at the latest use.
    pub version: u32,
}

/// A narration event cited as evidence, and the words in it that support
/// the fact (none for an encounter, which the whole narration supports).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub event: i64,
    pub excerpt: Option<String>,
}

impl Source {
    /// A whole narration event as evidence.
    pub fn event(event: i64) -> Source {
        Source {
            event,
            excerpt: None,
        }
    }
}

// A bare event number (as places were first recorded) or
// `{event, excerpt}`.
impl Serialize for Source {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Full<'a> {
            event: i64,
            excerpt: &'a str,
        }
        match &self.excerpt {
            None => s.serialize_i64(self.event),
            Some(excerpt) => Full {
                event: self.event,
                excerpt,
            }
            .serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for Source {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Source, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Event(i64),
            Full {
                event: i64,
                #[serde(default)]
                excerpt: Option<String>,
            },
        }
        Ok(match Repr::deserialize(d)? {
            Repr::Event(event) => Source::event(event),
            Repr::Full { event, excerpt } => Source { event, excerpt },
        })
    }
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
    /// The place entry where a person or item was last seen, as of
    /// `last_seen`. Changed only by an encounter, so off-screen movement
    /// never moves it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    seen_at: Option<EntityId>,
    #[serde(default)]
    facts: Vec<Fact>,
    /// The rule or glossary term a mechanic is about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    link: Option<Link>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    usage: Option<Usage>,
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
    /// The place entry where it was last seen (people and items).
    pub seen_at: Option<EntityId>,
    /// Whether the entry is linked to a world entity. Which one is never
    /// exposed: readers show only what was learned.
    pub linked: bool,
    pub facts: Vec<Fact>,
    /// What a mechanic is about in the engine, when established. Readers
    /// never follow it.
    pub link: Option<Link>,
    pub usage: Option<Usage>,
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
            seen_at: r.seen_at,
            linked: r.subject.is_some(),
            facts: r.facts,
            link: r.link,
            usage: r.usage,
        }
    }

    /// Whether this is learned terminology (a mechanic about a glossary
    /// term) rather than an interaction.
    pub fn is_term(&self) -> bool {
        matches!(self.link, Some(Link::Term(_)))
    }

    /// Fact numbers (1-based) that a later fact corrects.
    pub fn superseded(&self) -> Vec<usize> {
        superseded(&self.facts)
    }

    /// Whether `needle` (lowercase) occurs in the entry's name, summary,
    /// facts or who claimed them.
    fn mentions(&self, needle: &str) -> bool {
        self.name.to_lowercase().contains(needle)
            || self.summary.to_lowercase().contains(needle)
            || self.facts.iter().any(|f| {
                f.text.to_lowercase().contains(needle)
                    || f.claimed_by
                        .as_ref()
                        .is_some_and(|c| c.to_lowercase().contains(needle))
            })
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

/// The Mechanics section as the player knows it: interactions (how things
/// work, including those learned from a rule) and terminology.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Mechanics {
    pub interactions: Vec<Entry>,
    pub terms: Vec<Entry>,
}

/// Every learned mechanic, split into interactions and terms, in the order
/// first learned.
pub fn mechanics(world: &World) -> Result<Mechanics, StoreError> {
    let (terms, interactions) = entries(world)?
        .into_iter()
        .filter(|e| e.category == Category::Mechanic)
        .partition(Entry::is_term);
    Ok(Mechanics {
        interactions,
        terms,
    })
}

/// The entry linked to `link`, if any.
fn linked(world: &World, link: &Link) -> Result<Option<(EntityId, Record)>, StoreError> {
    Ok(world
        .query::<Record>()?
        .into_iter()
        .find(|(_, r)| r.link.as_ref() == Some(link)))
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
            if r.encounters == 0 {
                // Known by mention until now: this is its first sighting.
                r.first_seen = source;
            }
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
                seen_at: None,
                facts,
                link: None,
                usage: None,
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

/// Remember the people and notable items the turn's narration (event
/// `source`) showed the player: entities directly in the player's room or
/// carried by the player, that the narration names (whole words, any
/// case). Being named in the scene is the encounter's whole justification,
/// so nothing else about them is read or copied.
///
/// - An entity some entry is linked to gives that entry an encounter: one
///   more sighting, last seen now, at the place the player is in.
/// - An NPC (`acts`) no entry is linked to gets a new person entry linked
///   to it, unless an unlinked person entry of the same name is known:
///   whether that is the same subject only an explicit link can say, so
///   the engine waits for one rather than merge or duplicate by name.
/// - Items gain entries only through `learn` (with `subject`), which makes
///   them notable; after that their encounters are recorded here.
///
/// Returns the entries encountered. Nothing is written when nothing was
/// encountered, or the source isn't narration.
pub(crate) fn record_encounters(
    world: &mut World,
    source: i64,
) -> Result<Vec<EntityId>, StoreError> {
    if check_source(world, source)?.is_err() {
        return Ok(Vec::new());
    }
    let text = normalize(
        world
            .event(source)?
            .and_then(|e| e.payload["text"].as_str().map(str::to_owned))
            .as_deref()
            .unwrap_or_default(),
    );
    let Some(&player) = world.entities_of_kind(world::PLAYER_KIND)?.first() else {
        return Ok(Vec::new());
    };
    let Some(room) = world.entity(player)?.and_then(|p| p.location).filter(|&r| {
        world
            .entity(r)
            .ok()
            .flatten()
            .is_some_and(|e| e.kind == ROOM_KIND)
    }) else {
        return Ok(Vec::new());
    };
    let named = |name: &str| contains_words(&text, &normalize(name));
    let records = world.query::<Record>()?;
    let mut nearby = world.contents(Some(room))?;
    nearby.extend(world.contents(Some(player))?);

    let mut seen: Vec<(Option<EntityId>, Record)> = Vec::new();
    let mut met: Vec<String> = Vec::new();
    for id in nearby {
        let Some(e) = world.entity(id)? else { continue };
        if id == player || world::is_entry(&e) || world::RESERVED_KINDS.contains(&e.kind.as_str()) {
            continue;
        }
        let linked: Vec<&(EntityId, Record)> = records
            .iter()
            .filter(|(_, r)| {
                r.subject == Some(id) && matches!(r.category, Category::Person | Category::Item)
            })
            .collect();
        if !linked.is_empty() {
            for (entry_id, r) in linked {
                let entry_name = world.entity(*entry_id)?.map(|e| e.name).unwrap_or_default();
                if named(&e.name) || named(&entry_name) {
                    seen.push((Some(*entry_id), r.clone()));
                }
            }
            continue;
        }
        let acts = e.props.get(world::ACTS) == Some(&serde_json::Value::Bool(true));
        if !acts || !named(&e.name) {
            continue;
        }
        let mut waiting = false;
        for (entry_id, r) in &records {
            if r.category == Category::Person
                && r.subject.is_none()
                && world
                    .entity(*entry_id)?
                    .is_some_and(|k| k.name.to_lowercase() == e.name.to_lowercase())
            {
                waiting = true;
            }
        }
        if !waiting
            && !met
                .iter()
                .any(|m| m.to_lowercase() == e.name.to_lowercase())
        {
            met.push(e.name.clone());
            seen.push((
                None,
                Record {
                    category: Category::Person,
                    subject: Some(id),
                    summary: String::new(),
                    summary_source: source,
                    first_seen: source,
                    last_seen: source,
                    encounters: 0,
                    seen_at: None,
                    facts: Vec::new(),
                    link: None,
                    usage: None,
                },
            ));
        }
    }
    if seen.is_empty() {
        return Ok(Vec::new());
    }
    // Where they were seen, as the player knows the place.
    let place = match about(world, Category::Place, room)? {
        Some((id, _)) => Some(id),
        None => record_place(world, room, None, source)?.ok(),
    };
    let mut ops = Vec::new();
    let mut existing = Vec::new();
    for (entry_id, mut r) in seen {
        if r.encounters == 0 {
            r.first_seen = source;
        }
        r.last_seen = source;
        r.encounters += 1;
        r.seen_at = place.or(r.seen_at);
        refresh(&mut r);
        let Some(entry_id) = entry_id else {
            let name = world
                .entity(r.subject.expect("a new encounter has its subject"))?
                .map(|e| e.name)
                .unwrap_or_default();
            ops.push(Op::Create {
                handle: None,
                name,
                kind: KNOWLEDGE_KIND.into(),
                location: None,
                props: [world::prop(&r)].into_iter().collect(),
            });
            continue;
        };
        existing.push(entry_id);
        ops.push(world::set_op(entry_id, &r));
    }
    match world.apply_engine_patch(&Patch::new(ops)) {
        Ok(applied) => {
            existing.extend(applied.created);
            Ok(existing)
        }
        Err(ApplyError::Rejected(_)) => Ok(Vec::new()),
        Err(ApplyError::Storage(e)) => Err(StoreError::Sqlite(e)),
    }
}

/// Add an observed fact, or cite `source` for it again if already known.
fn add_fact(facts: &mut Vec<Fact>, text: String, source: i64) {
    merge_fact(
        facts,
        Fact {
            text,
            kind: FactKind::Observed,
            claimed_by: None,
            claimant: None,
            sources: vec![Source::event(source)],
            supersedes: Vec::new(),
            rule_version: None,
        },
    );
}

/// Add `fact`, or cite its sources for the same fact (same words, kind and
/// attribution) already known.
fn merge_fact(facts: &mut Vec<Fact>, fact: Fact) {
    let same = |f: &&mut Fact| {
        f.text == fact.text && f.kind == fact.kind && f.claimed_by == fact.claimed_by
    };
    match facts.iter_mut().find(same) {
        Some(f) => {
            for s in fact.sources {
                if !f.sources.iter().any(|k| k.event == s.event) {
                    f.sources.push(s);
                }
            }
            if f.claimant.is_none() {
                f.claimant = fact.claimant;
            }
            for n in fact.supersedes {
                if !f.supersedes.contains(&n) {
                    f.supersedes.push(n);
                }
            }
        }
        None => facts.push(fact),
    }
    // A repeat of a fact can't correct itself.
    for (i, f) in facts.iter_mut().enumerate() {
        f.supersedes.retain(|&n| n != i + 1);
    }
}

/// A discovery to remember: facts about one entry, each citing the
/// narration that supports it. The `learn` tool's input, and what the
/// one-off reconstruction script writes through [`learn`] too.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Discovery {
    /// An existing entry to add to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<EntityId>,
    /// A new entry's name and category (instead of `entry`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<Category>,
    /// Start a new entry even though one of the same name and category is
    /// known: a different subject that shares the name.
    #[serde(default, deserialize_with = "null_is_false")]
    pub distinct: bool,
    /// The world entity the entry is about, when it exists physically.
    /// With `entry`, links an entry known until now only by mention to the
    /// entity it turned out to be.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<EntityId>,
    /// For a mechanic: the rule it is about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<RuleId>,
    /// For a mechanic: the glossary term it is about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub term: Option<String>,
    /// May be empty with `entry` and `subject` (a known person or item):
    /// a link and nothing more.
    #[serde(default)]
    pub facts: Vec<NewFact>,
}

impl Discovery {
    /// The engine link the discovery names, if any.
    fn link(&self) -> Option<Link> {
        match (&self.rule, &self.term) {
            (Some(r), _) => Some(Link::Rule(*r)),
            (None, Some(t)) => Some(Link::Term(t.trim().to_owned())),
            (None, None) => None,
        }
    }
}

/// `distinct` as given, with `null` read as not given.
fn null_is_false<'de, D: serde::Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    Ok(Option::<bool>::deserialize(d)?.unwrap_or(false))
}

/// One fact of a [`Discovery`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewFact {
    pub text: String,
    /// Words from the cited narration that support the fact.
    pub excerpt: String,
    /// The narration event cited; none for the narration still to come
    /// (the current turn's).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<i64>,
    /// Who said it, for a claim; none for what the player observed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_by: Option<String>,
    /// The entry for whoever said it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimant: Option<EntityId>,
    /// Earlier facts of the entry that this one corrects, by number.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supersedes: Vec<usize>,
}

/// Fewest words an excerpt may have, so it pins down a passage.
const MIN_EXCERPT_WORDS: usize = 3;
/// Most characters an excerpt may have.
const MAX_EXCERPT_CHARS: usize = 400;

/// Text compared for an excerpt: lowercase, straight quotes and dashes,
/// no emphasis markers, single spaces.
fn normalize(s: &str) -> String {
    let s: String = s
        .chars()
        .filter(|&c| c != '*' && c != '_')
        .map(|c| match c {
            '‘' | '’' => '\'',
            '“' | '”' => '"',
            '–' | '—' => '-',
            c => c,
        })
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .replace('…', "...");
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// An excerpt as compared: normalised, without surrounding quotes or
/// ellipses.
fn normalize_excerpt(s: &str) -> String {
    normalize(s)
        .trim_matches(|c: char| c == '"' || c == '\'' || c == '.' || c.is_whitespace())
        .to_owned()
}

/// Check that `excerpt` occurs in narration event `seq`. `Err` says why
/// not.
fn check_excerpt(world: &World, seq: i64, excerpt: &str) -> Result<Result<(), String>, StoreError> {
    if let Err(why) = check_source(world, seq)? {
        return Ok(Err(why));
    }
    let event = world.event(seq)?.expect("checked above");
    let text = normalize(event.payload["text"].as_str().unwrap_or_default());
    if contains_words(&text, &normalize_excerpt(excerpt)) {
        Ok(Ok(()))
    } else {
        Ok(Err(format!(
            "narration #{seq} does not say {excerpt:?}; quote it word for word"
        )))
    }
}

/// Whether `needle` occurs in `text` starting and ending on word
/// boundaries (so "nk ties" doesn't match inside "monk ties").
fn contains_words(text: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let word = |c: Option<char>| c.is_some_and(char::is_alphanumeric);
    text.match_indices(needle).any(|(i, m)| {
        let before = text[..i].chars().next_back();
        let after = text[i + m.len()..].chars().next();
        let first = m.chars().next();
        let last = m.chars().next_back();
        !(word(before) && word(first)) && !(word(after) && word(last))
    })
}

/// What a discovery adds to.
enum Target {
    Existing(EntityId, Record),
    New { name: String, category: Category },
}

/// Check a discovery against the store, citing `current` for facts with no
/// source of their own. With `current` `None` (staging, before the turn's
/// narration exists), those facts' excerpts are left unchecked. `pending`
/// are the discoveries already staged this turn.
pub(crate) fn check(
    world: &World,
    d: &Discovery,
    current: Option<i64>,
    pending: &[Discovery],
) -> Result<Result<(), String>, StoreError> {
    Ok(resolve(world, d, current, pending)?.map(|_| ()))
}

fn resolve(
    world: &World,
    d: &Discovery,
    current: Option<i64>,
    pending: &[Discovery],
) -> Result<Result<Target, String>, StoreError> {
    macro_rules! refuse {
        ($($t:tt)*) => { return Ok(Err(format!($($t)*))) };
    }
    let all = entries(world)?;
    let target = match (d.entry, &d.name, d.category) {
        (Some(_), None, None) if d.distinct => {
            refuse!("distinct goes with a new entry's name and category, not entry")
        }
        (Some(id), None, None) => match world.get::<Record>(id)? {
            Some(r) => Target::Existing(id, r),
            None => refuse!("#{id} is not a compendium entry"),
        },
        (Some(_), ..) => {
            refuse!(
                "give entry to add to a known entry, or name and category for a new one, not both"
            )
        }
        (None, Some(name), Some(category)) => {
            let name = name.trim();
            if name.is_empty() {
                refuse!("a new entry needs a name");
            }
            let same: Vec<String> = all
                .iter()
                .filter(|e| e.category == category && e.name.to_lowercase() == name.to_lowercase())
                .map(|e| {
                    let gist = e
                        .facts
                        .first()
                        .map_or(e.summary.as_str(), |f| f.text.as_str());
                    format!("#{} ({})", e.id, crate::xray::clip(gist, 80))
                })
                .collect();
            let staged = pending.iter().any(|p| {
                p.entry.is_none()
                    && p.category == Some(category)
                    && p.name
                        .as_deref()
                        .is_some_and(|n| n.trim().to_lowercase() == name.to_lowercase())
            });
            if !d.distinct && !same.is_empty() {
                refuse!(
                    "a {} called {name:?} is already known as {}: add to it with entry, or \
                     pass distinct: true if this is a different one that shares the name",
                    category.label(),
                    same.join(", ")
                );
            }
            if !d.distinct && staged {
                // It has no id yet, so it can't be named with entry.
                refuse!(
                    "a {} called {name:?} is already being recorded this turn: give all its \
                     facts in that one call, or pass distinct: true if this is a different \
                     one that shares the name",
                    category.label()
                );
            }
            Target::New {
                name: name.to_owned(),
                category,
            }
        }
        (None, ..) => {
            refuse!("give entry (a known entry's id), or name and category for a new one")
        }
    };

    if let Some(s) = d.subject {
        match world.entity(s)? {
            None => refuse!("subject #{s} does not exist"),
            Some(e) if world::is_entry(&e) => {
                refuse!("subject #{s} is a compendium entry, not a world entity")
            }
            Some(e) if e.kind != ROOM_KIND && world::RESERVED_KINDS.contains(&e.kind.as_str()) => {
                refuse!("subject #{s} is engine state, not something to learn about")
            }
            Some(_) => {}
        }
        let (own, category) = match &target {
            Target::Existing(id, r) => {
                if r.subject.is_some_and(|t| t != s) {
                    refuse!("entry #{id} is already about another entity");
                }
                (Some(*id), r.category)
            }
            Target::New { category, .. } => (None, *category),
        };
        if let Some((other, _)) = about(world, category, s)?
            && Some(other) != own
        {
            refuse!("entry #{other} is already about #{s}: add to it with entry");
        }
        // Another call this turn already records this entity elsewhere:
        // only one of them could be kept.
        for p in pending.iter().filter(|p| p.subject == Some(s)) {
            let same_entry = own.is_some() && p.entry == own;
            let p_category = match p.entry {
                Some(id) => world.get::<Record>(id)?.map(|r| r.category),
                None => p.category,
            };
            if !same_entry && p_category == Some(category) {
                refuse!(
                    "another learn call this turn already records #{s}; give all its facts \
                     in that one call"
                );
            }
        }
    }

    if let Err(why) = check_link(world, d, &target, pending)? {
        return Ok(Err(why));
    }

    if d.facts.is_empty() {
        // A link and nothing more: only for a known person or item, so
        // nothing new (not even a name) is written without a citation.
        match (&target, d.subject) {
            (Target::Existing(id, r), Some(s)) => {
                if r.subject == Some(s) {
                    refuse!("entry #{id} is already linked to #{s}: nothing to learn");
                }
                if !matches!(r.category, Category::Person | Category::Item) {
                    refuse!("only a person or item entry links to a world entity without facts");
                }
            }
            (Target::Existing(..), None) => {
                refuse!("a discovery needs at least one fact, or a subject to link")
            }
            (Target::New { .. }, _) => {
                refuse!("a new entry needs at least one fact; to link a known one, give entry")
            }
        }
    }
    let known_facts = match &target {
        Target::Existing(_, r) => r.facts.len(),
        Target::New { .. } => 0,
    };
    for f in &d.facts {
        if f.text.trim().is_empty() {
            refuse!("a fact needs text");
        }
        let excerpt = normalize_excerpt(&f.excerpt);
        if excerpt.split_whitespace().count() < MIN_EXCERPT_WORDS {
            refuse!(
                "excerpt {:?} is too short: quote at least {MIN_EXCERPT_WORDS} words of narration",
                f.excerpt
            );
        }
        if f.excerpt.chars().count() > MAX_EXCERPT_CHARS {
            refuse!("an excerpt is at most {MAX_EXCERPT_CHARS} characters");
        }
        if f.claimed_by.as_ref().is_some_and(|c| c.trim().is_empty()) {
            refuse!("claimed_by names who said it");
        }
        if let Some(&n) = f.supersedes.iter().find(|&&n| n == 0 || n > known_facts) {
            refuse!(
                "supersedes {n}: the entry has {known_facts} fact(s) learned before this \
                 turn; name them by number (recall lists them)"
            );
        }
        if f.claimant.is_some() && f.claimed_by.is_none() {
            refuse!("claimant goes with claimed_by");
        }
        if let Some(c) = f.claimant
            && world.get::<Record>(c)?.is_none()
        {
            refuse!("claimant #{c} is not a compendium entry");
        }
        // With no source and no `current`, the fact cites this turn's
        // narration, still to come: checked when it is committed.
        if let Some(seq) = f.source.or(current)
            && let Err(why) = check_excerpt(world, seq, &f.excerpt)?
        {
            return Ok(Err(why));
        }
    }
    Ok(Ok(target))
}

/// Check a discovery's `rule` or `term` link: only for a mechanic, naming
/// something that exists, and not already the link of another entry (or of
/// another discovery staged this turn). `Err` says why not.
fn check_link(
    world: &World,
    d: &Discovery,
    target: &Target,
    pending: &[Discovery],
) -> Result<Result<(), String>, StoreError> {
    macro_rules! refuse {
        ($($t:tt)*) => { return Ok(Err(format!($($t)*))) };
    }
    if d.rule.is_some() && d.term.is_some() {
        refuse!("a mechanic is about a rule or a glossary term, not both");
    }
    let Some(link) = d.link() else {
        return Ok(Ok(()));
    };
    let (own, category, current) = match target {
        Target::Existing(id, r) => (Some(*id), r.category, r.link.as_ref()),
        Target::New { category, .. } => (None, *category, None),
    };
    if category != Category::Mechanic {
        refuse!(
            "rule and term go with a mechanic, not a {}",
            category.label()
        );
    }
    match &link {
        Link::Rule(id) => {
            if rules::get(world, *id)?.is_none() {
                refuse!("there is no rule #{id}");
            }
        }
        Link::Term(name) => {
            if !glossary::terms(world)?.contains_key(name) {
                refuse!("{name:?} is not a glossary term");
            }
        }
    }
    if let (Some(id), Some(other)) = (own, current)
        && *other != link
    {
        refuse!("entry #{id} is already about another rule or term");
    }
    let what = match &link {
        Link::Rule(id) => format!("rule #{id}"),
        Link::Term(name) => format!("the term {name:?}"),
    };
    if let Some((other, _)) = linked(world, &link)?
        && Some(other) != own
    {
        refuse!("entry #{other} is already about {what}: add to it with entry");
    }
    for p in pending.iter().filter(|p| p.link().as_ref() == Some(&link)) {
        if own.is_none() || p.entry != own {
            refuse!(
                "another learn call this turn already records {what}; give all its facts in \
                 that one call"
            );
        }
    }
    // Another call this turn links the same entry elsewhere: only the first
    // could be kept, and the other's facts would be lost.
    if let Some(id) = own
        && pending
            .iter()
            .any(|p| p.entry == Some(id) && p.link().is_some_and(|l| l != link))
    {
        refuse!("another learn call this turn links entry #{id} to another rule or term");
    }
    Ok(Ok(()))
}

/// `d` with only the facts whose excerpts their narration contains
/// (citing `current` where they give no source), or `None` when no fact
/// is left.
pub(crate) fn supported(
    world: &World,
    d: &Discovery,
    current: i64,
) -> Result<Option<Discovery>, StoreError> {
    if d.facts.is_empty() {
        // A link only: the turn narrated, which is all it needs.
        return Ok(Some(d.clone()));
    }
    let mut kept = d.clone();
    kept.facts.clear();
    for f in &d.facts {
        if check_excerpt(world, f.source.unwrap_or(current), &f.excerpt)?.is_ok() {
            kept.facts.push(f.clone());
        }
    }
    Ok((!kept.facts.is_empty()).then_some(kept))
}

/// Remember `d`, citing narration event `current` for facts that give no
/// source of their own. Every citation is checked first (the event exists,
/// is narration the player saw, and contains the excerpt); a refusal
/// writes nothing and leaves what was known as it was. Returns the entry's
/// id, or `Err` with why nothing was written.
///
/// A matching excerpt proves the citation exists, not that the fact
/// follows from it: callers state only what the narration says.
pub(crate) fn learn(
    world: &mut World,
    d: &Discovery,
    current: Option<i64>,
) -> Result<Result<EntityId, String>, StoreError> {
    if current.is_none() && d.facts.iter().any(|f| f.source.is_none()) {
        return Ok(Err("a fact cites no narration".into()));
    }
    let target = match resolve(world, d, current, &[])? {
        Ok(t) => t,
        Err(why) => return Ok(Err(why)),
    };
    let link = match &target {
        Target::Existing(_, r) => r.link.clone().or_else(|| d.link()),
        Target::New { .. } => d.link(),
    };
    let rule_version = match &link {
        Some(Link::Rule(id)) => rules::get(world, *id)?.map(|r| r.version),
        _ => None,
    };
    let facts = d.facts.iter().map(|f| Fact {
        text: f.text.trim().to_owned(),
        kind: if f.claimed_by.is_some() {
            FactKind::Claimed
        } else {
            FactKind::Observed
        },
        claimed_by: f.claimed_by.as_ref().map(|c| c.trim().to_owned()),
        claimant: f.claimant,
        sources: vec![Source {
            event: f.source.or(current).expect("checked above"),
            excerpt: Some(f.excerpt.trim().to_owned()),
        }],
        supersedes: f.supersedes.clone(),
        rule_version,
    });
    let (op, existing) = match target {
        Target::Existing(id, mut r) => {
            for f in facts {
                merge_fact(&mut r.facts, f);
            }
            if r.subject.is_none() {
                r.subject = d.subject;
            }
            if r.link.is_none() {
                r.link = link;
            }
            refresh(&mut r);
            (world::set_op(id, &r), Some(id))
        }
        Target::New { name, category } => {
            let mut r = Record {
                category,
                subject: d.subject,
                summary: String::new(),
                summary_source: 0,
                first_seen: 0,
                last_seen: 0,
                encounters: 0,
                seen_at: None,
                facts: Vec::new(),
                link,
                usage: None,
            };
            for f in facts {
                merge_fact(&mut r.facts, f);
            }
            refresh(&mut r);
            let op = Op::Create {
                handle: None,
                name,
                kind: KNOWLEDGE_KIND.into(),
                location: None,
                props: [world::prop(&r)].into_iter().collect(),
            };
            (op, None)
        }
    };
    match world.apply_engine_patch(&Patch::new(vec![op])) {
        Ok(applied) => Ok(Ok(existing.unwrap_or_else(|| applied.created[0]))),
        Err(ApplyError::Rejected(r)) => Ok(Err(format!("patch rejected: {r}"))),
        Err(ApplyError::Storage(e)) => Err(StoreError::Sqlite(e)),
    }
}

/// Fact numbers (1-based) that a later fact corrects.
fn superseded(facts: &[Fact]) -> Vec<usize> {
    let mut out: Vec<usize> = facts.iter().flat_map(|f| f.supersedes.clone()).collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Bring an entry's span and summary up to date with its facts. The
/// summary is the observed facts nothing has corrected (claims stay
/// attributed, and nothing beyond the facts goes in); an encountered place
/// keeps the description it was found with instead. An entry never
/// encountered spans the narration its facts cite.
fn refresh(r: &mut Record) {
    let cited = |facts: &[&Fact]| {
        facts
            .iter()
            .flat_map(|f| f.sources.iter().map(|s| s.event))
            .collect::<Vec<_>>()
    };
    if r.encounters == 0 {
        let all = cited(&r.facts.iter().collect::<Vec<_>>());
        r.first_seen = all.iter().copied().min().unwrap_or(0);
        r.last_seen = all.iter().copied().max().unwrap_or(0);
    }
    if r.category == Category::Place && r.encounters > 0 {
        return;
    }
    let gone = superseded(&r.facts);
    let observed: Vec<&Fact> = r
        .facts
        .iter()
        .enumerate()
        .filter(|(i, f)| f.kind == FactKind::Observed && !gone.contains(&(i + 1)))
        .map(|(_, f)| f)
        .collect();
    r.summary = observed
        .iter()
        .map(|f| f.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    r.summary_source = cited(&observed).into_iter().max().unwrap_or(r.first_seen);
}

/// Remember that the player used the mechanic learned about rule `rule`
/// (at version `version`), as narrated by `source`: its usage is counted
/// and cited. Nothing else is copied from the rule, so its conditions and
/// effects stay unknown. Returns the entry, or `None` when no entry is
/// about the rule (a rule the player never learned stays unlearned) or the
/// source isn't narration.
pub(crate) fn record_use(
    world: &mut World,
    rule: RuleId,
    version: u32,
    source: i64,
) -> Result<Option<EntityId>, StoreError> {
    let Some((id, mut r)) = linked(world, &Link::Rule(rule))? else {
        return Ok(None);
    };
    if check_source(world, source)?.is_err() {
        return Ok(None);
    }
    r.usage = Some(match r.usage {
        Some(u) => Usage {
            count: u.count + 1,
            last: source,
            version,
            ..u
        },
        None => Usage {
            count: 1,
            first: source,
            last: source,
            version,
        },
    });
    match world.apply_engine_patch(&Patch::new(vec![world::set_op(id, &r)])) {
        Ok(_) => Ok(Some(id)),
        Err(ApplyError::Rejected(_)) => Ok(None),
        Err(ApplyError::Storage(e)) => Err(StoreError::Sqlite(e)),
    }
}

/// The entry learned about rule `rule` and its current facts, numbered as
/// `supersedes` names them, for the director when it changes the rule: the
/// compendium keeps what the player learned until a narrated fact
/// supersedes it.
pub(crate) fn learned_about_rule(
    world: &World,
    rule: RuleId,
) -> Result<Option<serde_json::Value>, StoreError> {
    let Some((id, r)) = linked(world, &Link::Rule(rule))? else {
        return Ok(None);
    };
    let gone = superseded(&r.facts);
    let facts: Vec<serde_json::Value> = r
        .facts
        .iter()
        .enumerate()
        .filter(|(i, _)| !gone.contains(&(i + 1)))
        .map(|(i, f)| {
            let mut v = serde_json::json!({"fact": i + 1, "text": f.text});
            if let Some(n) = f.rule_version {
                v["learned_at_rule_version"] = serde_json::json!(n);
            }
            v
        })
        .collect();
    Ok(Some(serde_json::json!({"entry": id, "facts": facts})))
}
