//! The world tick: between turns, one batched director call advances
//! off-screen arcs (NPCs scheme, factions move) through ordinary patches.
//!
//! When a tick is due is worked out from the event log alone, so it
//! survives saving and resuming: every [`TICK_EVERY`] player turns, or
//! when the player has changed rooms since the last tick (at least
//! [`ROOM_CHANGE_GAP`] turns after it). Each tick is recorded as a `tick`
//! event listing its notes and the rooms they touched; the local slice
//! shows a room's notes when the player is back in it.

use std::collections::BTreeSet;
use std::fmt::Write;

use serde_json::{Value, json};

use crate::llm::{Effort, LlmClient, Message, Model, Request, ToolOutput, ToolUse};
use crate::world::{EntityId, EntityRef, Event, EventKind, Patch, StoreError, World};

use super::tools::{Phase, definitions, op_refs};
use super::{Game, GameError, ROOM_KIND, bible_id, player_id};

/// A tick is due after this many player turns without one.
pub const TICK_EVERY: usize = 5;
/// After a room change, a tick is due once this many turns have passed
/// since the last one, so walking every turn doesn't tick every turn.
pub const ROOM_CHANGE_GAP: usize = 2;
/// Most model calls one tick may make.
const MAX_TICK_CALLS: usize = 10;
/// Earlier tick summaries the tick slice shows, for continuity.
const PRIOR_TICKS: usize = 2;
/// Recent player inputs the tick slice shows.
const RECENT_INPUTS: usize = 6;

const TICK_SYSTEM: &str = "You are the director of Loam, a text adventure, running a world \
tick between turns. The world keeps moving while the player is elsewhere: advance one to \
three off-screen arcs by a small, concrete step, following the bible's factions, NPCs and \
secrets and reacting to what the player has done. Make every change with apply_patch and a \
one-sentence note; leave traces the player can find later (an NPC moved, a door barred, a \
letter left). You may create entities inside off-screen rooms but not new rooms. The player, \
the player's room and everything in them are off-limits. Doing nothing is fine when nothing \
would plausibly happen. When done, reply with a one- to three-sentence summary of what moved, \
for the log; the player never sees it.";

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
        let after = last.as_ref().map_or(0, |e| e.seq);
        let turns = turns_since(world, after)?;
        let reference = match &last {
            Some(e) => e.payload.get("room").and_then(Value::as_i64),
            // Before the first tick: the opening room.
            None => world.entities_of_kind(ROOM_KIND)?.first().copied(),
        };
        Ok(turns >= TICK_EVERY || (Some(room) != reference && turns >= ROOM_CHANGE_GAP))
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
        let slice = tick_slice(&self.world)?;
        let request = Request::new(Model::Sonnet, vec![Message::user(slice)])
            .system(TICK_SYSTEM)
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
            .filter(|(_, applied)| *applied)
            .map(|(c, _)| c)
            .collect();
        self.world.log_event(
            EventKind::Tick,
            &json!({
                "turn": turn,
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

/// Collect the rooms an `apply_patch` call touches. Called once before the
/// call runs (`output` is `None`) to record the rooms its entities are in
/// beforehand, and once after; an applied patch becomes a [`TickChange`].
fn track_change(
    world: &World,
    call: &ToolUse,
    output: Option<&ToolOutput>,
    changes: &mut Vec<(TickChange, bool)>,
) {
    if call.name != "apply_patch" {
        return;
    }
    let Ok(patch) = serde_json::from_value::<Patch>(call.input.clone()) else {
        return;
    };
    let mut ids: Vec<EntityId> = patch
        .ops
        .iter()
        .flat_map(op_refs)
        .filter_map(|r| match r {
            EntityRef::Id(id) => Some(*id),
            EntityRef::Handle(_) => None,
        })
        .collect();
    match output {
        None => {
            let note = call
                .input
                .get("note")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let rooms = rooms_of(world, &ids);
            // Pending until the call's result says it applied.
            changes.push((TickChange { note, rooms }, false));
        }
        Some(output) => {
            let Some((change, done)) = changes.last_mut().filter(|(_, done)| !done) else {
                return;
            };
            let ToolOutput::Ok(result) = output else {
                changes.pop();
                return;
            };
            if let Ok(v) = serde_json::from_str::<Value>(result)
                && let Some(created) = v.get("created").and_then(Value::as_array)
            {
                ids.extend(created.iter().filter_map(Value::as_i64));
            }
            let mut rooms: BTreeSet<EntityId> = change.rooms.iter().copied().collect();
            rooms.extend(rooms_of(world, &ids));
            change.rooms = rooms.into_iter().collect();
            *done = true;
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

fn enclosing_room(world: &World, id: EntityId) -> Result<Option<EntityId>, StoreError> {
    let mut at = Some(id);
    while let Some(here) = at {
        let Some(e) = world.entity(here)? else {
            return Ok(None);
        };
        if e.kind == ROOM_KIND {
            return Ok(Some(e.id));
        }
        at = e.location;
    }
    Ok(None)
}

fn player_room(world: &World) -> Result<Option<EntityId>, StoreError> {
    Ok(match player_id(world)? {
        Some(p) => world.entity(p)?.and_then(|p| p.location),
        None => None,
    })
}

fn last_tick(world: &World) -> Result<Option<Event>, StoreError> {
    Ok(world
        .search_events("", &[EventKind::Tick], 1)?
        .into_iter()
        .next())
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
        Some(bible) => bible.props.keys().cloned().collect::<Vec<_>>().join(", "),
        None => "(none)".into(),
    };
    writeln!(out, "World tick. Bible sections (read_bible): {sections}").unwrap();

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

    let rooms: Vec<String> = world
        .entities_of_kind(ROOM_KIND)?
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

    let mut npcs = Vec::new();
    for id in world.entities_of_kind("npc")? {
        let Some(npc) = world.entity(id)? else {
            continue;
        };
        let at = match enclosing_room(world, id)? {
            Some(r) if Some(r) == here => "with the player".to_owned(),
            Some(r) => format!("in #{r}"),
            None => "nowhere in particular".to_owned(),
        };
        npcs.push(format!("#{} {} {at}", npc.id, npc.name));
    }
    if !npcs.is_empty() {
        writeln!(out, "NPCs: {}", npcs.join("; ")).unwrap();
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
