//! The world clock: in-world minutes that every turn spends.
//!
//! The clock is a component on the bible entity, written through engine
//! patches, so replay and forking reproduce it. Each advance is also
//! logged as a `time` event saying how far it went and who decided, for
//! `:time` and the slice. A world with no clock yet (an old save, or one
//! before its first turn) reads as its start: day 1, 08:00 unless the
//! bible chose another hour.

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::world::{
    self, ApplyError, Component, EntityId, Event, EventKind, Patch, StoreError, World,
};

#[cfg(test)]
mod tests;

/// Minutes in a day.
pub const DAY: u64 = 24 * 60;
/// Where a clock with no start set begins: day 1, 08:00.
pub const DEFAULT_START: u64 = 8 * 60;
/// What a turn costs when nothing says otherwise.
pub const DEFAULT_MINUTES: u64 = 10;
/// Longest single advance: a week.
pub const MAX_ADVANCE: u64 = 7 * DAY;

/// The clock, on the bible: minutes since day 1, 00:00.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Clock {
    minute: u64,
}

impl Component for Clock {
    const KEY: &'static str = "_clock";
}

/// Who decided how long a turn took.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// Nothing did; the engine's default.
    Default,
    /// The Haiku interpreter's size estimate.
    Interpreter,
    /// The director, explicitly.
    Director,
    /// The rule the fast path applied.
    Rule,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Default => "default",
            Source::Interpreter => "interpreter",
            Source::Director => "director",
            Source::Rule => "rule",
        }
    }
}

/// A moment in world time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Moment(pub u64);

impl Moment {
    /// Day number, from 1.
    pub fn day(self) -> u64 {
        self.0 / DAY + 1
    }

    pub fn hour(self) -> u64 {
        self.0 % DAY / 60
    }

    pub fn minute(self) -> u64 {
        self.0 % 60
    }

    /// The part of the day, from the hour.
    pub fn part(self) -> &'static str {
        match self.hour() {
            5..=6 => "dawn",
            7..=11 => "morning",
            12..=16 => "afternoon",
            17..=18 => "dusk",
            19..=21 => "evening",
            _ => "night",
        }
    }
}

impl std::fmt::Display for Moment {
    /// `day 2, 18:40 (dusk)`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "day {}, {:02}:{:02} ({})",
            self.day(),
            self.hour(),
            self.minute(),
            self.part()
        )
    }
}

/// A span of minutes in words: `1 min`, `2 h 5 min`, `1 day 3 h`.
pub fn span(minutes: u64) -> String {
    let (d, h, m) = (minutes / DAY, minutes % DAY / 60, minutes % 60);
    let mut parts = Vec::new();
    if d > 0 {
        parts.push(format!("{d} day{}", if d == 1 { "" } else { "s" }));
    }
    if h > 0 {
        parts.push(format!("{h} h"));
    }
    if m > 0 || parts.is_empty() {
        parts.push(format!("{m} min"));
    }
    parts.join(" ")
}

/// Parse `"HH:MM"` (24-hour) into minutes after midnight.
pub fn parse_hhmm(s: &str) -> Option<u64> {
    let (h, m) = s.trim().split_once(':')?;
    if !(1..=2).contains(&h.len()) || m.len() != 2 {
        return None;
    }
    let (h, m): (u64, u64) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

/// The clock's prop entry for the bible's create op, starting on day 1 at
/// `start` minutes after midnight.
pub(crate) fn start_prop(start: u64) -> (String, serde_json::Value) {
    world::prop(&Clock {
        minute: start % DAY,
    })
}

fn bible(world: &World) -> Result<Option<EntityId>, StoreError> {
    Ok(world
        .entities_of_kind(crate::game::BIBLE_KIND)?
        .first()
        .copied())
}

/// The current world time.
pub fn now(world: &World) -> Result<Moment, StoreError> {
    let clock = match bible(world)? {
        Some(id) => world.get::<Clock>(id)?,
        None => None,
    };
    Ok(Moment(clock.map_or(DEFAULT_START, |c| c.minute)))
}

/// One logged advance of the clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advance {
    pub seq: i64,
    pub minutes: u64,
    pub source: String,
    /// The time afterwards.
    pub to: Moment,
}

impl Advance {
    fn from_event(e: &Event) -> Advance {
        let p = &e.payload;
        Advance {
            seq: e.seq,
            minutes: p["minutes"].as_u64().unwrap_or(0),
            source: p["source"].as_str().unwrap_or("?").to_owned(),
            to: Moment(p["minute"].as_u64().unwrap_or(0)),
        }
    }
}

/// Move the clock on by `minutes` (at most [`MAX_ADVANCE`]) and log it.
/// Does nothing before the bible exists. Returns the new time.
pub fn advance(world: &mut World, minutes: u64, source: Source) -> Result<Moment, StoreError> {
    let Some(id) = bible(world)? else {
        return now(world);
    };
    let minutes = minutes.min(MAX_ADVANCE);
    let to = now(world)?.0 + minutes;
    let patch = Patch::new(vec![world::set_op(id, &Clock { minute: to })]);
    match world.apply_engine_patch(&patch) {
        Ok(_) => {}
        Err(ApplyError::Rejected(r)) => {
            return Err(StoreError::Corrupt(format!("clock advance rejected: {r}")));
        }
        Err(ApplyError::Storage(e)) => return Err(e.into()),
    }
    world.log_event(
        EventKind::Time,
        &json!({"minutes": minutes, "source": source, "minute": to}),
    )?;
    Ok(Moment(to))
}

/// The most recent advances, newest first, at most `limit`.
pub fn recent(world: &World, limit: usize) -> Result<Vec<Advance>, StoreError> {
    Ok(world
        .search_events("", &[EventKind::Time], limit)?
        .iter()
        .map(Advance::from_event)
        .collect())
}

/// The first logged advance, if the clock has moved at all.
pub fn first(world: &World) -> Result<Option<Advance>, StoreError> {
    Ok(world
        .first_event(EventKind::Time)?
        .as_ref()
        .map(Advance::from_event))
}

/// The slice's time line: `Time: day 1, 08:10 (morning); your last action
/// took 10 min`.
pub fn slice_line(world: &World) -> Result<String, StoreError> {
    let mut line = format!("Time: {}", now(world)?);
    if let Some(last) = recent(world, 1)?.first() {
        line.push_str(&format!("; your last action took {}", span(last.minutes)));
    }
    Ok(line)
}
