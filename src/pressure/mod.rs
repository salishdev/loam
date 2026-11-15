//! Pressures: soft countdowns the director starts and the world leans on.
//!
//! A pressure is something that will happen unless someone acts ("the
//! sluice gives way", due in 6 hours, flooding the lower mill). The engine
//! tracks how close each one is and shows the active ones to the director
//! and the world tick, nearest first. It never makes the consequence happen
//! itself: when a pressure is overdue the slice nudges the director to
//! advance or resolve it, and the director decides how. Nothing here ever
//! refuses a player action.
//!
//! A pressure is a top-level world entity of kind [`PRESSURE_KIND`], its
//! state in the engine-owned `_pressure` component and the entities it
//! threatens as [`CONCERNS_REL`] relations (pressure → entity), all written
//! by engine patches, so the event log, replay and forking cover pressures
//! like anything else. Urgency is derived from the world clock and never
//! stored.

pub(crate) mod tools;

#[cfg(test)]
mod tests;

pub use tools::{handle_tool, tool_definitions};

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Map;

use crate::quest::CONCERNS_REL;
use crate::time::{self, Moment};
use crate::world::{
    self, ApplyError, Component, EntityId, EntityRef, Op, Patch, StoreError, World,
};

/// Kind of pressure entities.
pub const PRESSURE_KIND: &str = "pressure";

/// Most entities one pressure may concern.
pub const MAX_CONCERNS: usize = 8;
/// Furthest a pressure may be due, and the most one adjustment may move it.
pub const MAX_DUE_IN: u64 = 30 * time::DAY;
/// A pressure with this little time left is imminent.
pub const IMMINENT_MINUTES: u64 = 60;

/// The nudge an overdue pressure carries in the director and tick slices.
pub const OVERDUE_NUDGE: &str =
    "overdue: advance or resolve it this turn, or say in the tick why not";

/// A pressure's state: the `_pressure` component.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct State {
    what: String,
    consequence: String,
    /// World minute it started.
    started: u64,
    /// World minute it is due.
    due: u64,
    status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resolution: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    adjustments: Vec<Adjustment>,
}

impl Component for State {
    const KEY: &'static str = "_pressure";
}

/// One change to a pressure's due time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Adjustment {
    /// World minute it was made.
    pub at: u64,
    /// How far the due time moved: positive delays, negative hastens.
    pub minutes: i64,
    pub reason: String,
}

/// Whether a pressure still bears on the world.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Active,
    /// It happened, or it was defused; the resolution says which.
    Resolved,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Active => "active",
            Status::Resolved => "resolved",
        }
    }
}

/// How close an active pressure is, derived from the clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Urgency {
    /// More than half the span from start to due is left.
    Low,
    /// Half or less is left, and more than [`IMMINENT_MINUTES`].
    Rising,
    /// At most [`IMMINENT_MINUTES`] left.
    Imminent,
    /// The due time has come.
    Overdue,
}

impl Urgency {
    pub fn as_str(self) -> &'static str {
        match self {
            Urgency::Low => "low",
            Urgency::Rising => "rising",
            Urgency::Imminent => "imminent",
            Urgency::Overdue => "overdue",
        }
    }
}

/// The urgency of a pressure running from `started` to `due`, at `now`.
pub fn urgency(started: u64, due: u64, now: u64) -> Urgency {
    if now >= due {
        return Urgency::Overdue;
    }
    let left = due - now;
    if left <= IMMINENT_MINUTES {
        Urgency::Imminent
    } else if left * 2 > due.saturating_sub(started) {
        Urgency::Low
    } else {
        Urgency::Rising
    }
}

/// A pressure as read from the world store.
#[derive(Debug, Clone, PartialEq)]
pub struct Pressure {
    pub id: EntityId,
    /// What is coming.
    pub what: String,
    /// What happens when it comes.
    pub consequence: String,
    pub started: Moment,
    pub due: Moment,
    pub status: Status,
    /// How it ended, once resolved.
    pub resolution: Option<String>,
    /// Changes to the due time, oldest first.
    pub adjustments: Vec<Adjustment>,
    /// Entities it threatens, in id order.
    pub concerns: Vec<EntityId>,
}

impl Pressure {
    fn new(id: EntityId, state: State, concerns: Vec<EntityId>) -> Pressure {
        Pressure {
            id,
            what: state.what,
            consequence: state.consequence,
            started: Moment(state.started),
            due: Moment(state.due),
            status: state.status,
            resolution: state.resolution,
            adjustments: state.adjustments,
            concerns,
        }
    }

    fn state(&self) -> State {
        State {
            what: self.what.clone(),
            consequence: self.consequence.clone(),
            started: self.started.0,
            due: self.due.0,
            status: self.status,
            resolution: self.resolution.clone(),
            adjustments: self.adjustments.clone(),
        }
    }

    /// How close it is at `now`.
    pub fn urgency(&self, now: Moment) -> Urgency {
        urgency(self.started.0, self.due.0, now.0)
    }

    /// `due day 1, 14:00 (in 2 h)` or `due day 1, 14:00 (20 min ago)`.
    pub fn due_text(&self, now: Moment) -> String {
        let rel = if self.due.0 > now.0 {
            format!("in {}", time::span(self.due.0 - now.0))
        } else if self.due.0 == now.0 {
            "now".to_owned()
        } else {
            format!("{} ago", time::span(now.0 - self.due.0))
        };
        format!("due {} ({rel})", self.due)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PressureError {
    /// The change is not allowed; the reason is meant for the director.
    #[error("{0}")]
    Refused(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

type Result<T, E = PressureError> = std::result::Result<T, E>;

fn refuse<T>(reason: impl Into<String>) -> Result<T> {
    Err(PressureError::Refused(reason.into()))
}

fn engine_patch(world: &mut World, ops: Vec<Op>) -> Result<crate::world::Applied> {
    match world.apply_engine_patch(&Patch::new(ops)) {
        Ok(applied) => Ok(applied),
        Err(ApplyError::Rejected(r)) => refuse(format!("patch rejected: {r}")),
        Err(ApplyError::Storage(e)) => Err(StoreError::Sqlite(e).into()),
    }
}

/// Every pressure, in the order started.
pub fn pressures(world: &World) -> Result<Vec<Pressure>, StoreError> {
    world
        .query::<State>()?
        .into_iter()
        .map(|(id, state)| load(world, id, state))
        .collect()
}

/// Active pressures, nearest due first (ties in the order started).
pub fn active(world: &World) -> Result<Vec<Pressure>, StoreError> {
    let mut all: Vec<Pressure> = pressures(world)?
        .into_iter()
        .filter(|p| p.status == Status::Active)
        .collect();
    all.sort_by_key(|p| (p.due, p.id));
    Ok(all)
}

/// One pressure by id, if `id` is a pressure.
pub fn pressure(world: &World, id: EntityId) -> Result<Option<Pressure>, StoreError> {
    world
        .get::<State>(id)?
        .map(|state| load(world, id, state))
        .transpose()
}

fn load(world: &World, id: EntityId, state: State) -> Result<Pressure, StoreError> {
    let concerns = world
        .relations_from(id, Some(CONCERNS_REL))?
        .into_iter()
        .map(|r| r.object)
        .collect();
    Ok(Pressure::new(id, state, concerns))
}

/// The pressure `id`, or a refusal naming the problem.
fn existing(world: &World, id: EntityId) -> Result<Pressure> {
    match pressure(world, id)? {
        Some(p) => Ok(p),
        None => refuse(format!("#{id} is not a pressure")),
    }
}

/// Check concerned entity ids: they must exist and be things in the world
/// or quests. Returns them deduplicated, in the order given.
pub(crate) fn check_concerns(world: &World, ids: &[EntityId]) -> Result<Vec<EntityId>> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for &id in ids {
        let Some(e) = world.entity(id)? else {
            return refuse(format!("entity #{id} does not exist"));
        };
        if matches!(
            e.kind.as_str(),
            PRESSURE_KIND
                | crate::game::BIBLE_KIND
                | crate::rules::RULE_KIND
                | crate::glossary::GLOSSARY_KIND
        ) {
            return refuse(format!(
                "#{id} is a {}; a pressure concerns things in the world or a quest",
                e.kind
            ));
        }
        if seen.insert(id) {
            out.push(id);
        }
    }
    if out.len() > MAX_CONCERNS {
        return refuse(format!(
            "a pressure may concern at most {MAX_CONCERNS} entities"
        ));
    }
    Ok(out)
}

fn not_blank(field: &str, value: &str) -> Result<String> {
    let v = value.trim();
    if v.is_empty() {
        return refuse(format!("{field} must not be empty"));
    }
    Ok(v.to_owned())
}

/// Start a pressure: `what` will happen with `consequence` in
/// `due_in_minutes` of world time unless someone acts.
pub fn start(
    world: &mut World,
    what: &str,
    consequence: &str,
    due_in_minutes: u64,
    concerns: &[EntityId],
) -> Result<EntityId> {
    let what = not_blank("what", what)?;
    let consequence = not_blank("consequence", consequence)?;
    if !(1..=MAX_DUE_IN).contains(&due_in_minutes) {
        return refuse(format!(
            "due_in_minutes must be 1 to {MAX_DUE_IN} ({})",
            time::span(MAX_DUE_IN)
        ));
    }
    let concerns = check_concerns(world, concerns)?;
    let now = time::now(world)?.0;
    let state = State {
        what: what.clone(),
        consequence,
        started: now,
        due: now + due_in_minutes,
        status: Status::Active,
        resolution: None,
        adjustments: Vec::new(),
    };
    let mut ops = vec![Op::Create {
        handle: Some("pressure".into()),
        name: what,
        kind: PRESSURE_KIND.into(),
        location: None,
        props: Map::from_iter([world::prop(&state)]),
    }];
    ops.extend(concerns.iter().map(|&id| Op::Relate {
        subject: EntityRef::Handle("pressure".into()),
        rel: CONCERNS_REL.into(),
        object: id.into(),
        props: Map::new(),
    }));
    let applied = engine_patch(world, ops)?;
    Ok(applied.created[0])
}

/// The active pressure `id`, or a refusal.
fn active_one(world: &World, id: EntityId) -> Result<Pressure> {
    let p = existing(world, id)?;
    if p.status != Status::Active {
        return refuse(format!(
            "pressure #{id} is already resolved: {}",
            p.resolution.as_deref().unwrap_or("")
        ));
    }
    Ok(p)
}

/// Move an active pressure's due time: positive `minutes` delay it,
/// negative hasten it. Hastening stops at now (it is then overdue).
pub fn adjust(world: &mut World, id: EntityId, minutes: i64, reason: &str) -> Result<Pressure> {
    let p = active_one(world, id)?;
    let reason = not_blank("reason", reason)?;
    if minutes == 0 || minutes.unsigned_abs() > MAX_DUE_IN {
        return refuse(format!(
            "minutes must be non-zero and at most {MAX_DUE_IN} either way"
        ));
    }
    let now = time::now(world)?.0;
    let due = (p.due.0 as i64 + minutes).max(now as i64) as u64;
    let mut state = p.state();
    state.adjustments.push(Adjustment {
        at: now,
        minutes: due as i64 - p.due.0 as i64,
        reason,
    });
    state.due = due;
    engine_patch(world, vec![world::set_op(id, &state)])?;
    existing(world, id)
}

/// Mark an active pressure resolved: it happened, or it was defused.
pub fn resolve(world: &mut World, id: EntityId, how: &str) -> Result<Pressure> {
    let p = active_one(world, id)?;
    let how = not_blank("how", how)?;
    let mut state = p.state();
    state.status = Status::Resolved;
    state.resolution = Some(how);
    engine_patch(world, vec![world::set_op(id, &state)])?;
    existing(world, id)
}

/// One line per active pressure, nearest first, for the director's and the
/// tick's slices. `mark` may append a note to a pressure's line (the tick
/// marks on-screen ones). Empty when nothing presses.
pub fn slice_lines(
    world: &World,
    mut mark: impl FnMut(&Pressure) -> Result<Option<String>, StoreError>,
) -> Result<Vec<String>, StoreError> {
    let now = time::now(world)?;
    let mut lines = Vec::new();
    for p in active(world)? {
        let urgency = p.urgency(now);
        let mut line = format!(
            "- pressure #{} {} [{}] {}; then: {}",
            p.id,
            p.what,
            urgency.as_str(),
            p.due_text(now),
            p.consequence
        );
        if !p.concerns.is_empty() {
            let ids: Vec<String> = p.concerns.iter().map(|id| format!("#{id}")).collect();
            line.push_str(&format!(" (concerns {})", ids.join(" ")));
        }
        if let Some(m) = mark(&p)? {
            line.push_str(&format!(" ({m})"));
        }
        if urgency == Urgency::Overdue {
            line.push_str(&format!(". {OVERDUE_NUDGE}"));
        }
        lines.push(line);
    }
    Ok(lines)
}
