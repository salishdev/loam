//! Where the world's NPCs are: shared by the tick director's slice and the
//! `:npcs` inspector, so both see the same list.

use serde_json::Value;

use crate::world::{Entity, EntityId, StoreError, World};

use super::{ROOM_KIND, bible_id, player_id};

/// The entity kind NPCs are created with.
pub const NPC_KIND: &str = "npc";

/// One NPC entity and where it is.
#[derive(Debug, Clone, PartialEq)]
pub struct NpcSighting {
    pub npc: Entity,
    /// The room it is in, through any containers or carriers.
    pub room: Option<EntityId>,
    /// Whether that room is the player's.
    pub with_player: bool,
}

/// Every NPC entity, in id order, with where it is.
pub fn npcs(world: &World) -> Result<Vec<NpcSighting>, StoreError> {
    let here = player_room(world)?;
    let mut out = Vec::new();
    for id in world.entities_of_kind(NPC_KIND)? {
        let Some(npc) = world.entity(id)? else {
            continue;
        };
        let room = enclosing_room(world, id)?;
        out.push(NpcSighting {
            npc,
            room,
            with_player: room.is_some() && room == here,
        });
    }
    Ok(out)
}

/// The names in the bible's `npcs` section, in bible order. An item is an
/// object with a `name` or a bare string; anything else is skipped.
pub fn bible_npc_names(world: &World) -> Result<Vec<String>, StoreError> {
    let Some(bible) = bible_id(world)?
        .map(|id| world.entity(id))
        .transpose()?
        .flatten()
    else {
        return Ok(Vec::new());
    };
    let Some(items) = bible.props.get("npcs").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    Ok(items
        .iter()
        .filter_map(|item| match item {
            Value::String(s) => Some(s.as_str()),
            _ => item.get("name").and_then(Value::as_str),
        })
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect())
}

/// The room an entity is in, walking out through whatever holds it.
pub(super) fn enclosing_room(world: &World, id: EntityId) -> Result<Option<EntityId>, StoreError> {
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

/// The room the player stands in, once they are somewhere.
pub(super) fn player_room(world: &World) -> Result<Option<EntityId>, StoreError> {
    Ok(match player_id(world)? {
        Some(p) => world.entity(p)?.and_then(|p| p.location),
        None => None,
    })
}
