//! Exits and stub rooms.
//!
//! Every exit is an [`EXIT_REL`] relation from one room to another with
//! props `{dir, label}`. An exit nobody has gone through yet leads to a
//! *stub*: a room marked [`Unexplored`], named from the hint, with nothing
//! in it. `create_room` fills a stub in when the player first goes there;
//! `move_player` refuses to enter one. Listings of places skip stubs.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::world::{Component, EXIT_REL, Entity, EntityId, EntityRef, Op, StoreError, World};

use super::ROOM_KIND;

/// Engine-owned marker on a stub room.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct Unexplored(pub(crate) bool);

impl Component for Unexplored {
    const KEY: &'static str = "_unexplored";
}

/// One exit of a room.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Exit {
    pub dir: String,
    pub label: String,
    pub to: EntityId,
    /// Whether `to` is a stub.
    pub unexplored: bool,
}

/// Whether `e` is a stub room.
pub(crate) fn is_stub_entity(e: &Entity) -> Result<bool, StoreError> {
    Ok(e.kind == ROOM_KIND && e.get::<Unexplored>()?.is_some_and(|u| u.0))
}

/// Whether `id` is a stub room.
pub(crate) fn is_stub(world: &World, id: EntityId) -> Result<bool, StoreError> {
    match world.entity(id)? {
        Some(e) => is_stub_entity(&e),
        None => Ok(false),
    }
}

/// Every room that isn't a stub, in id order.
pub(crate) fn explored_rooms(world: &World) -> Result<Vec<EntityId>, StoreError> {
    let mut out = Vec::new();
    for id in world.entities_of_kind(ROOM_KIND)? {
        if !is_stub(world, id)? {
            out.push(id);
        }
    }
    Ok(out)
}

/// The exits of `room`, ordered by direction.
pub(crate) fn exits(world: &World, room: EntityId) -> Result<Vec<Exit>, StoreError> {
    let mut out = Vec::new();
    for r in world.relations_from(room, Some(EXIT_REL))? {
        let text = |k: &str| {
            r.props
                .get(k)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned()
        };
        out.push(Exit {
            dir: text("dir"),
            label: text("label"),
            to: r.object,
            unexplored: is_stub(world, r.object)?,
        });
    }
    out.sort_by(|a, b| a.dir.cmp(&b.dir));
    Ok(out)
}

/// `north, up`, or `none`.
pub(crate) fn dir_names(exits: &[Exit]) -> String {
    if exits.is_empty() {
        "none".into()
    } else {
        exits
            .iter()
            .map(|e| e.dir.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// An op relating `from -exit-> to`.
pub(crate) fn relate_op(
    from: impl Into<EntityRef>,
    to: impl Into<EntityRef>,
    dir: &str,
    label: &str,
) -> Op {
    let Value::Object(props) = json!({"dir": dir, "label": label}) else {
        unreachable!()
    };
    Op::Relate {
        subject: from.into(),
        rel: EXIT_REL.into(),
        object: to.into(),
        props,
    }
}

/// An op creating a stub room under `handle`, named from `hint`.
pub(crate) fn stub_op(handle: String, hint: &str) -> Op {
    Op::Create {
        handle: Some(handle),
        name: hint.to_owned(),
        kind: ROOM_KIND.into(),
        location: None,
        props: Map::from_iter([crate::world::prop(&Unexplored(true))]),
    }
}
