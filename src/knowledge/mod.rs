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
//! Two writers: [`record_place`] for rooms the player enters, and [`learn`]
//! for discoveries (observations and attributed claims about any category,
//! including subjects only mentioned, which need no world entity). The
//! director submits discoveries with the `learn` tool; they are staged
//! until the turn's narration exists, then committed ([`tools`]). Every
//! write cites player-visible source events (non-empty `narration`, and for
//! a discovery the excerpt it quotes), checked before anything is written.
//! The queries read entries only, so nothing the player hasn't learned can
//! show up in a list, a name match or a search.

mod report;
pub(crate) mod tools;

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
    merge_fact(
        facts,
        Fact {
            text,
            kind: FactKind::Observed,
            claimed_by: None,
            claimant: None,
            sources: vec![Source::event(source)],
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
        }
        None => facts.push(fact),
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<EntityId>,
    pub facts: Vec<NewFact>,
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

    if d.facts.is_empty() {
        refuse!("a discovery needs at least one fact");
    }
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

/// `d` with only the facts whose excerpts their narration contains
/// (citing `current` where they give no source), or `None` when no fact
/// is left.
pub(crate) fn supported(
    world: &World,
    d: &Discovery,
    current: i64,
) -> Result<Option<Discovery>, StoreError> {
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
    });
    let (op, existing) = match target {
        Target::Existing(id, mut r) => {
            for f in facts {
                merge_fact(&mut r.facts, f);
            }
            if r.subject.is_none() {
                r.subject = d.subject;
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
                facts: Vec::new(),
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

/// Bring a learned entry's span and summary up to date with its facts.
/// An entry never encountered is summarised from its observations only
/// (claims stay attributed, and nothing beyond the facts goes in); an
/// encountered place keeps the description it was found with.
fn refresh(r: &mut Record) {
    // An encountered entry's first and last sightings stay its encounters.
    if r.encounters == 0 {
        let cited = || {
            r.facts
                .iter()
                .flat_map(|f| f.sources.iter().map(|s| s.event))
        };
        r.first_seen = cited().min().unwrap_or(0);
        r.last_seen = cited().max().unwrap_or(0);
        let observed: Vec<&Fact> = r
            .facts
            .iter()
            .filter(|f| f.kind == FactKind::Observed)
            .collect();
        r.summary = observed
            .iter()
            .map(|f| f.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        r.summary_source = observed
            .iter()
            .flat_map(|f| f.sources.iter().map(|s| s.event))
            .max()
            .unwrap_or(r.first_seen);
    }
}
