//! The local slice: the little the director sees up front each turn.
//!
//! Current room and its exits, what is in it (one level down), what the
//! player carries, the relations of everything in the room, what world
//! ticks did in the room, the last few turns and the bible's section titles.
//! Everything else is fetched through tools.

use std::collections::BTreeMap;
use std::fmt::Write;

use serde_json::Value;

use crate::stats;
use crate::world::{ENGINE_PROP_PREFIX, Entity, EntityId, EventKind, Relation, StoreError, World};
use crate::{glossary, rules};

use super::tick::notes_for_room;
use super::tools::exits_of;
use super::{bible_id, player_id};

/// How many recent inputs and narrations the slice shows.
const RECENT_EVENTS: usize = 6;
/// Most world-tick notes shown for the current room.
const ROOM_NOTES: usize = 3;
/// Longest recent narration shown in full.
const RECENT_CHARS: usize = 500;

/// Render the local slice as compact text.
pub fn local_slice(world: &World) -> Result<String, StoreError> {
    let mut out = String::new();

    let sections = match bible_id(world)?
        .map(|id| world.entity(id))
        .transpose()?
        .flatten()
    {
        Some(bible) => bible.props.keys().cloned().collect::<Vec<_>>().join(", "),
        None => "(none)".into(),
    };
    writeln!(out, "Bible sections (read_bible): {sections}").unwrap();
    writeln!(
        out,
        "Glossary (lookup_terms; ? = undefined): {}",
        glossary::index(world)?
    )
    .unwrap();
    let rules = rules::active_world(world)?.len();
    if rules > 0 {
        writeln!(out, "Rules: {rules} active (list_rules)").unwrap();
    }

    let player = match player_id(world)? {
        Some(id) => world.entity(id)?,
        None => None,
    };
    let Some(player) = player else {
        writeln!(out, "No player yet.").unwrap();
        return Ok(out);
    };
    writeln!(out, "Player: {}", line(&player)).unwrap();
    let stats = crate::stats::slice_lines(world)?;
    if !stats.is_empty() {
        writeln!(out, "Stats (check, adjust_stat):").unwrap();
        for s in stats {
            writeln!(out, "{s}").unwrap();
        }
    }
    let carried = world.contents(Some(player.id))?;
    if carried.is_empty() {
        writeln!(out, "Carrying: nothing").unwrap();
    } else {
        writeln!(out, "Carrying:").unwrap();
        tree(world, &carried, 1, &mut out)?;
    }

    match player
        .location
        .map(|id| world.entity(id))
        .transpose()?
        .flatten()
    {
        None => writeln!(out, "Location: nowhere yet").unwrap(),
        Some(room) => room_view(world, &room, player.id, &mut out)?,
    }

    let quests = crate::quest::slice_lines(world)?;
    if !quests.is_empty() {
        writeln!(out, "Quests:").unwrap();
        for q in quests {
            writeln!(out, "{q}").unwrap();
        }
    }

    // The seed is logged as an input; it isn't part of play.
    let mut recent: Vec<_> = world
        .search_events(
            "",
            &[EventKind::Input, EventKind::Narration],
            RECENT_EVENTS + 1,
        )?
        .into_iter()
        .filter(|e| e.payload.get("seed").is_none())
        .take(RECENT_EVENTS)
        .collect();
    recent.reverse();
    if !recent.is_empty() {
        writeln!(out, "Recent:").unwrap();
        for e in recent {
            let text = e
                .payload
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default();
            match e.kind {
                EventKind::Input => writeln!(out, "> {text}").unwrap(),
                _ => writeln!(out, "{}", clip(text, RECENT_CHARS)).unwrap(),
            }
        }
    }
    Ok(out)
}

fn room_view(
    world: &World,
    room: &Entity,
    player: EntityId,
    out: &mut String,
) -> Result<(), StoreError> {
    let region = room
        .props
        .get("region")
        .and_then(Value::as_str)
        .unwrap_or("?");
    writeln!(
        out,
        "Location: #{} {} (region: {region})",
        room.id, room.name
    )
    .unwrap();
    let description = room
        .props
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("");
    writeln!(out, "Description: {description}").unwrap();
    let other: serde_json::Map<String, Value> = room
        .props
        .iter()
        .filter(|(k, _)| !matches!(k.as_str(), "description" | "region" | "exits"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if !other.is_empty() {
        writeln!(out, "Room props: {}", Value::Object(other)).unwrap();
    }
    let exits = exits_of(room);
    if exits.is_empty() {
        writeln!(out, "Exits: none").unwrap();
    } else {
        let list: Vec<String> = exits
            .iter()
            .map(|(dir, e)| {
                let label = e.get("label").and_then(Value::as_str).unwrap_or("");
                match e.get("to").and_then(Value::as_i64) {
                    Some(to) => format!("{dir} -> #{to} {label}"),
                    None => format!("{dir} -> unexplored ({label})"),
                }
            })
            .collect();
        writeln!(out, "Exits: {}", list.join("; ")).unwrap();
    }
    let here: Vec<EntityId> = world
        .contents(Some(room.id))?
        .into_iter()
        .filter(|id| *id != player)
        .collect();
    if here.is_empty() {
        writeln!(out, "Here: nothing").unwrap();
    } else {
        writeln!(out, "Here:").unwrap();
        tree(world, &here, 1, out)?;
    }
    let relations = room_relations(world, room.id)?;
    if !relations.is_empty() {
        writeln!(out, "Relations here:").unwrap();
        for r in relations {
            writeln!(out, "- {}", relation_line(world, &r)?).unwrap();
        }
    }
    let notes = notes_for_room(world, room.id, ROOM_NOTES)?;
    if !notes.is_empty() {
        writeln!(out, "Happened here off-screen (world ticks, newest first):").unwrap();
        for note in notes {
            writeln!(out, "- {note}").unwrap();
        }
    }
    Ok(())
}

/// Every director-visible relation either end of which is the room or
/// something inside it (at any depth, the player included), in key order.
/// Engine-owned (`_`) relations are left to their own modules.
fn room_relations(world: &World, room: EntityId) -> Result<Vec<Relation>, StoreError> {
    let mut found = BTreeMap::new();
    let mut stack = vec![room];
    while let Some(id) = stack.pop() {
        let both = world
            .relations_from(id, None)?
            .into_iter()
            .chain(world.relations_to(id, None)?);
        for r in both {
            if !r.rel.starts_with(ENGINE_PROP_PREFIX) {
                found.insert((r.subject, r.rel.clone(), r.object), r);
            }
        }
        stack.extend(world.contents(Some(id))?);
    }
    Ok(found.into_values().collect())
}

/// `#3 Hound -guards-> #4 Gate {props}`.
fn relation_line(world: &World, r: &Relation) -> Result<String, StoreError> {
    let name = |id: EntityId| -> Result<String, StoreError> {
        Ok(match world.entity(id)? {
            Some(e) => format!("#{id} {}", e.name),
            None => format!("#{id}"),
        })
    };
    let mut s = format!("{} -{}-> {}", name(r.subject)?, r.rel, name(r.object)?);
    if !r.props.is_empty() {
        write!(s, " {}", Value::Object(r.props.clone())).unwrap();
    }
    Ok(s)
}

/// List entities with props; their contents one level further down.
fn tree(world: &World, ids: &[EntityId], depth: usize, out: &mut String) -> Result<(), StoreError> {
    for id in ids {
        let Some(e) = world.entity(*id)? else {
            continue;
        };
        writeln!(out, "{}- {}", "  ".repeat(depth - 1), line(&e)).unwrap();
        let inner = world.contents(Some(e.id))?;
        if depth < 2 {
            tree(world, &inner, depth + 1, out)?;
        } else if !inner.is_empty() {
            writeln!(
                out,
                "{}  (contains {} more; query_entity #{})",
                "  ".repeat(depth - 1),
                inner.len(),
                e.id
            )
            .unwrap();
        }
    }
    Ok(())
}

fn line(e: &Entity) -> String {
    // The sheet is shown on its own lines.
    let props: serde_json::Map<String, Value> = e
        .props
        .iter()
        .filter(|(k, _)| !stats::is_sheet_key(k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if props.is_empty() {
        format!("#{} {} [{}]", e.id, e.name, e.kind)
    } else {
        format!("#{} {} [{}] {}", e.id, e.name, e.kind, Value::Object(props))
    }
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_owned()
    } else {
        let cut: String = text.chars().take(max).collect();
        format!("{cut}…")
    }
}
