//! Free-form state patches the director (or the engine) writes.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Engine-owned entity id.
pub type EntityId = i64;

/// A reference to an entity inside a patch: an existing id, or the
/// `handle` of an entity created by an earlier op in the same patch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EntityRef {
    Id(EntityId),
    Handle(String),
}

impl From<EntityId> for EntityRef {
    fn from(id: EntityId) -> Self {
        EntityRef::Id(id)
    }
}

impl From<&str> for EntityRef {
    fn from(handle: &str) -> Self {
        EntityRef::Handle(handle.to_owned())
    }
}

impl std::fmt::Display for EntityRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EntityRef::Id(id) => write!(f, "#{id}"),
            EntityRef::Handle(h) => write!(f, "'{h}'"),
        }
    }
}

/// One change in a patch. Ops apply in order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    /// Create an entity. `location: None` puts it at the top level.
    Create {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        handle: Option<String>,
        name: String,
        kind: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        location: Option<EntityRef>,
        #[serde(default, skip_serializing_if = "Map::is_empty")]
        props: Map<String, Value>,
    },
    /// Move an entity into a container, or to the top level when `to` is `None`.
    Move {
        entity: EntityRef,
        to: Option<EntityRef>,
    },
    SetProp {
        entity: EntityRef,
        key: String,
        value: Value,
    },
    RemoveProp {
        entity: EntityRef,
        key: String,
    },
    Rename {
        entity: EntityRef,
        name: String,
    },
    /// Remove an entity, its props and every relation it is subject or
    /// object of. It must not contain anything.
    Destroy {
        entity: EntityRef,
    },
    /// State that `subject` stands in relation `rel` to `object`
    /// (directional). Replaces the props if the relation already holds.
    Relate {
        subject: EntityRef,
        rel: String,
        object: EntityRef,
        #[serde(default, skip_serializing_if = "Map::is_empty")]
        props: Map<String, Value>,
    },
    /// Remove a relation, which must hold.
    Unrelate {
        subject: EntityRef,
        rel: String,
        object: EntityRef,
    },
}

/// An atomic batch of ops: either all apply or none do.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Patch {
    pub ops: Vec<Op>,
}

impl Patch {
    pub fn new(ops: Vec<Op>) -> Self {
        Patch { ops }
    }
}
