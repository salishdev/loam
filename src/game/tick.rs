//! The world tick: between turns, one batched director call advances
//! off-screen arcs (NPCs scheme, factions move) through ordinary patches.
//!
//! When a tick is due is worked out from the event log alone, so it
//! survives saving and resuming: once [`TICK_MINUTES`] of world time have
//! passed since the last tick, or when the player has changed rooms since
//! it and at least [`ROOM_CHANGE_MINUTES`] have passed. However long the
//! gap (a night's sleep), one tick runs, told how much time passed. Each
//! tick is recorded as a `tick` event with the clock's `minute`, listing
//! its notes and the rooms they touched; the local slice shows a room's
//! notes when the player is back in it.

use std::collections::BTreeSet;
use std::fmt::Write;

use serde_json::{Value, json};

use crate::llm::{Effort, LlmClient, Message, Model, Request, ToolOutput, ToolUse};
use crate::world::{EntityId, EntityRef, Event, EventKind, Patch, StoreError, World};

use super::exits::explored_rooms;
use super::npcs::{enclosing_room, npcs, player_room};
use super::tools::{Phase, definitions, op_refs};
use super::{Game, GameError, bible_id};
use crate::time;

/// A tick is due after this many minutes of world time without one.
pub const TICK_MINUTES: u64 = 60;
/// After a room change, a tick is due once this many minutes have passed
/// since the last one, so walking every turn doesn't tick every turn.
pub const ROOM_CHANGE_MINUTES: u64 = 20;
/// Most model calls one tick may make.
const MAX_TICK_CALLS: usize = 10;
/// Earlier tick summaries the tick slice shows, for continuity.
const PRIOR_TICKS: usize = 2;
/// Recent player inputs the tick slice shows.
const RECENT_INPUTS: usize = 6;

/// The tick's system prompt. The number of arcs scales with the world
/// time that passed, bounded by the tick's call budget.
fn tick_system() -> String {
    format!(
        "You are the director of Loam, a text adventure, running a world tick between turns. \
The world keeps moving while the player is elsewhere: advance off-screen arcs by small, \
concrete steps, following the bible's factions, NPCs and secrets and reacting to what the \
player has done. Scale the work to the time that passed, which the slice states: one to three \
arcs for about an hour; for long gaps (a night's sleep, a day's journey) more arcs and bigger \
steps, as far as that much time would plausibly carry them. You have at most {MAX_TICK_CALLS} \
tool calls. Make every change with apply_patch and a one-sentence note; leave traces the player \
can find later (an NPC moved, a door barred, a letter left). You may create entities inside \
off-screen rooms but not new rooms. The player, the player's room and everything in them are \
off-limits. Pressures listed in the slice may be advanced, delayed or resolved \
when they concern only off-screen things; an overdue one should move or be resolved, or your \
summary should say why not. Doing nothing is fine when nothing would plausibly happen. When done, reply with a \
one- to three-sentence summary of what moved, for the log; the player never sees it."
    )
}

/// What one world tick did.
#[derive(Debug, Clone, PartialEq)]
pub struct TickOutcome {
    /// The director's summary, for the log.
    pub summary: String,
    /// Each applied change: its note and the rooms it touched.
    pub changes: Vec<TickChange>,
    /// Model calls the tick made.
    pub calls: usize,
}

/// One off-screen patch the tick applied.
#[derive(Debug, Clone, PartialEq)]
pub struct TickChange {
    pub note: String,
    /// Rooms the patch touched, before or after applying it (an NPC that
    /// leaves a room touches both rooms).
    pub rooms: Vec<EntityId>,
}

impl Game {
    /// Whether a world tick is due now (see the module docs).
    pub fn tick_due(&self) -> Result<bool, GameError> {
        let world = &self.world;
        let Some(room) = player_room(world)? else {
            return Ok(false);
        };
        let last = last_tick(world)?;
        let elapsed = minutes_since_tick(world, last.as_ref())?;
        let reference = match &last {
            Some(e) => e.payload.get("room").and_then(Value::as_i64),
            // Before the first tick: the opening room.
            None => explored_rooms(world)?.first().copied(),
        };
        Ok(elapsed >= TICK_MINUTES || (Some(room) != reference && elapsed >= ROOM_CHANGE_MINUTES))
    }

    /// Run a world tick if one is due.
    pub fn maybe_tick(&mut self, client: &dyn LlmClient) -> Result<Option<TickOutcome>, GameError> {
        if self.tick_due()? {
            self.tick(client).map(Some)
        } else {
            Ok(None)
        }
    }

    /// Run a world tick now: one batched director call that advances
    /// off-screen arcs with ordinary patches. Nothing is streamed.
    pub fn tick(&mut self, client: &dyn LlmClient) -> Result<TickOutcome, GameError> {
        if bible_id(&self.world)?.is_none() {
            return Err(GameError::NotStarted);
        }
        let room = player_room(&self.world)?;
        let turn = turns_since(&self.world, 0)?;
        let minute = time::now(&self.world)?.0;
        let slice = tick_slice(&self.world)?;
        let request = Request::new(Model::Sonnet, vec![Message::user(slice)])
            .system(tick_system())
            .effort(Effort::Medium)
            .tools(definitions(Phase::Tick));
        let mut changes = Vec::new();
        let outcome = self.run(
            client,
            request,
            Phase::Tick,
            MAX_TICK_CALLS,
            &mut |_| {},
            Some(
                &mut |world: &World, call: &ToolUse, before: Option<&ToolOutput>| {
                    track_change(world, call, before, &mut changes)
                },
            ),
        )?;
        let summary = outcome.final_response.text().trim().to_owned();
        let changes: Vec<TickChange> = changes
            .into_iter()
            .filter(|(_, pending)| pending.is_none())
            .map(|(c, _)| c)
            .collect();
        self.world.log_event(
            EventKind::Tick,
            &json!({
                "turn": turn,
                "minute": minute,
                "room": room,
                "summary": summary,
                "changes": changes.iter().map(|c| json!({"note": c.note, "rooms": c.rooms})).collect::<Vec<_>>(),
            }),
        )?;
        Ok(TickOutcome {
            summary,
            changes,
            calls: outcome.calls,
        })
    }
}

/// A tick change in progress: the change so far and, until the call's
/// result says it applied, the last event before the call.
type Pending = (TickChange, Option<i64>);

/// Collect the rooms an `apply_patch` call touches. Called once before the
/// call runs (`output` is `None`) to record the rooms the patch's entities
/// are in beforehand (a move's origin isn't in the log), and once after,
/// when an applied patch adds the rooms of everything
/// [`World::changed_since`] the call began and becomes a [`TickChange`].
fn track_change(
    world: &World,
    call: &ToolUse,
    output: Option<&ToolOutput>,
    changes: &mut Vec<Pending>,
) {
    if call.name != "apply_patch" {
        return;
    }
    match output {
        None => {
            let Ok(patch) = serde_json::from_value::<Patch>(call.input.clone()) else {
                return;
            };
            let Ok(before) = world.last_seq() else {
                return;
            };
            let ids: Vec<EntityId> = patch
                .ops
                .iter()
                .flat_map(op_refs)
                .filter_map(|r| match r {
                    EntityRef::Id(id) => Some(*id),
                    EntityRef::Handle(_) => None,
                })
                .collect();
            let note = call
                .input
                .get("note")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let rooms = rooms_of(world, &ids);
            changes.push((TickChange { note, rooms }, Some(before)));
        }
        Some(output) => {
            let Some((change, pending)) = changes.last_mut() else {
                return;
            };
            let Some(before) = pending.take() else {
                return;
            };
            if !matches!(output, ToolOutput::Ok(_)) {
                changes.pop();
                return;
            }
            let Ok(changed) = world.changed_since(before) else {
                return;
            };
            let changed: Vec<EntityId> = changed.into_iter().collect();
            let mut rooms: BTreeSet<EntityId> = change.rooms.iter().copied().collect();
            rooms.extend(rooms_of(world, &changed));
            change.rooms = rooms.into_iter().collect();
        }
    }
}

/// The distinct rooms enclosing `ids` (a room encloses itself). Entities
/// that no longer exist or sit outside any room are skipped.
fn rooms_of(world: &World, ids: &[EntityId]) -> Vec<EntityId> {
    let mut rooms = BTreeSet::new();
    for id in ids {
        if let Ok(Some(room)) = enclosing_room(world, *id) {
            rooms.insert(room);
        }
    }
    rooms.into_iter().collect()
}

fn last_tick(world: &World) -> Result<Option<Event>, StoreError> {
    Ok(world
        .search_events("", &[EventKind::Tick], 1)?
        .into_iter()
        .next())
}

/// World minutes since the tick `last`, or before any tick since the
/// clock's first advance began. A tick logged before ticks recorded the
/// clock counts as minute 0.
fn minutes_since_tick(world: &World, last: Option<&Event>) -> Result<u64, StoreError> {
    let now = time::now(world)?.0;
    let from = match last {
        Some(tick) => tick
            .payload
            .get("minute")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        None => match time::first(world)? {
            Some(first) => first.to.0.saturating_sub(first.minutes),
            None => now,
        },
    };
    Ok(now.saturating_sub(from))
}

/// Player turns (inputs other than the seed) logged after event `after`.
fn turns_since(world: &World, after: i64) -> Result<usize, StoreError> {
    Ok(world
        .events_since(after)?
        .iter()
        .filter(|e| e.kind == EventKind::Input && e.payload.get("seed").is_none())
        .count())
}

/// Tick notes that touched `room`, newest first, at most `limit`.
pub(crate) fn notes_for_room(
    world: &World,
    room: EntityId,
    limit: usize,
) -> Result<Vec<String>, StoreError> {
    let mut notes = Vec::new();
    // Ticks are few; scanning a bounded window of recent ones is enough.
    for tick in world.search_events("", &[EventKind::Tick], 20)? {
        let changes = tick.payload.get("changes").and_then(Value::as_array);
        for change in changes.into_iter().flatten().rev() {
            let touched = change
                .get("rooms")
                .and_then(Value::as_array)
                .is_some_and(|rs| rs.iter().any(|r| r.as_i64() == Some(room)));
            let note = change.get("note").and_then(Value::as_str).unwrap_or("");
            if touched && !note.is_empty() {
                notes.push(note.to_owned());
                if notes.len() == limit {
                    return Ok(notes);
                }
            }
        }
    }
    Ok(notes)
}

/// What the tick director sees up front: where the player is (off-limits),
/// the explored rooms, the NPCs and where they are, the last ticks and
/// what the player has been doing.
fn tick_slice(world: &World) -> Result<String, StoreError> {
    let mut out = String::new();
    let sections = match bible_id(world)?
        .map(|id| world.entity(id))
        .transpose()?
        .flatten()
    {
        Some(bible) => super::bible_sections(&bible),
        None => "(none)".into(),
    };
    writeln!(out, "World tick. Bible sections (read_bible): {sections}").unwrap();
    let last = last_tick(world)?;
    let since = if last.is_some() {
        "the last tick"
    } else {
        "the story began"
    };
    writeln!(
        out,
        "Time: {}. {} have passed since {since}.",
        time::now(world)?,
        time::span(minutes_since_tick(world, last.as_ref())?)
    )
    .unwrap();

    let here = player_room(world)?;
    match here.map(|id| world.entity(id)).transpose()?.flatten() {
        Some(room) => writeln!(
            out,
            "The player is in #{} {} (off-limits, with everything in it).",
            room.id, room.name
        )
        .unwrap(),
        None => writeln!(out, "The player is nowhere yet.").unwrap(),
    }

    let rooms: Vec<String> = explored_rooms(world)?
        .into_iter()
        .filter(|id| Some(*id) != here)
        .filter_map(|id| world.entity(id).transpose())
        .map(|r| {
            r.map(|r| {
                let region = r.props.get("region").and_then(Value::as_str).unwrap_or("?");
                format!("#{} {} ({region})", r.id, r.name)
            })
        })
        .collect::<Result<_, _>>()?;
    if rooms.is_empty() {
        writeln!(out, "Other explored rooms: none").unwrap();
    } else {
        writeln!(out, "Other explored rooms: {}", rooms.join("; ")).unwrap();
    }

    let npcs: Vec<String> = npcs(world)?
        .into_iter()
        .map(|s| {
            let at = match s.room {
                Some(_) if s.with_player => "with the player".to_owned(),
                Some(r) => format!("in #{r}"),
                None => "nowhere in particular".to_owned(),
            };
            format!("#{} {} {at}", s.npc.id, s.npc.name)
        })
        .collect();
    if !npcs.is_empty() {
        writeln!(out, "NPCs: {}", npcs.join("; ")).unwrap();
    }

    let pressures = crate::pressure::slice_lines(world, |p| {
        let Some(player) = super::player_id(world)? else {
            return Ok(None);
        };
        for &id in &p.concerns {
            if super::tools::is_on_screen(world, id, player, here)? {
                return Ok(Some("on-screen: leave it to play".into()));
            }
        }
        Ok(None)
    })?;
    if !pressures.is_empty() {
        writeln!(
            out,
            "Pressures (nearest first; adjust_pressure, resolve_pressure):"
        )
        .unwrap();
        for p in pressures {
            writeln!(out, "{p}").unwrap();
        }
    }

    let prior: Vec<String> = world
        .search_events("", &[EventKind::Tick], PRIOR_TICKS)?
        .into_iter()
        .rev()
        .filter_map(|e| {
            e.payload
                .get("summary")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        })
        .collect();
    if !prior.is_empty() {
        writeln!(out, "Earlier ticks:").unwrap();
        for s in prior {
            writeln!(out, "- {s}").unwrap();
        }
    }

    let mut inputs: Vec<String> = world
        .search_events("", &[EventKind::Input], RECENT_INPUTS + 1)?
        .into_iter()
        .filter(|e| e.payload.get("seed").is_none())
        .take(RECENT_INPUTS)
        .filter_map(|e| {
            e.payload
                .get("text")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect();
    inputs.reverse();
    if !inputs.is_empty() {
        writeln!(out, "Player's recent inputs:").unwrap();
        for i in inputs {
            writeln!(out, "> {i}").unwrap();
        }
    }
    writeln!(out, "Advance the off-screen world, then summarise.").unwrap();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::world::Op;

    #[test]
    fn tick_slice_lists_npcs_and_where_they_are() {
        let mut world = World::open_in_memory().unwrap();
        let ops: Vec<Op> = serde_json::from_value(json!([
            {"op": "create", "handle": "hall", "name": "Hall", "kind": "room"},
            {"op": "create", "handle": "cellar", "name": "Cellar", "kind": "room"},
            {"op": "create", "handle": "you", "name": "you", "kind": "player", "location": "hall"},
            {"op": "create", "handle": "rat", "name": "Rat", "kind": "rat", "location": "hall", "props": {"acts": true}},
            {"op": "create", "handle": "hal", "name": "Hal", "kind": "miller", "location": "cellar", "props": {"acts": true}},
            {"op": "create", "handle": "ghost", "name": "Ghost", "kind": "spirit", "props": {"acts": true}}
        ]))
        .unwrap();
        let a = world.apply_engine_patch(&Patch::new(ops)).unwrap();
        let h = |n: &str| a.handles[n];
        let slice = tick_slice(&world).unwrap();
        let want = format!(
            "NPCs: #{} Rat with the player; #{} Hal in #{}; #{} Ghost nowhere in particular\n",
            h("rat"),
            h("hal"),
            h("cellar"),
            h("ghost")
        );
        assert!(slice.contains(&want), "{slice}");
    }
}
