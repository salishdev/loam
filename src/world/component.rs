//! Typed components over engine-owned `_` props.
//!
//! A component is a Rust type stored as one engine-owned prop. Each engine
//! module keeps its component types and keys private, so it is the only
//! code that can read or write them; there is no runtime ownership check.
//! Writes still go through engine patches, so the event log, replay and
//! forking cover components like any other prop.

use rusqlite::OptionalExtension;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::{
    Applied, ApplyError, ENGINE_PROP_PREFIX, Entity, EntityId, EntityRef, Op, Patch, StoreError,
    World,
};

/// Engine state held in one engine-owned prop.
pub trait Component: Serialize + DeserializeOwned {
    /// The prop key. Always starts with `_` (checked at compile time).
    const KEY: &'static str;
}

/// The component's key, with the `_` prefix checked at compile time.
fn key<C: Component>() -> &'static str {
    const {
        let k = C::KEY.as_bytes();
        let p = ENGINE_PROP_PREFIX.as_bytes();
        assert!(
            k.len() > p.len() && k[0] == p[0],
            "component keys start with `_`"
        )
    };
    C::KEY
}

fn corrupt<C: Component>(id: EntityId, e: serde_json::Error) -> StoreError {
    StoreError::Corrupt(format!("entity {id} component {:?}: {e}", C::KEY))
}

fn decode<C: Component>(id: EntityId, value: &Value) -> Result<C, StoreError> {
    C::deserialize(value).map_err(|e| corrupt::<C>(id, e))
}

fn decode_str<C: Component>(id: EntityId, raw: &str) -> Result<C, StoreError> {
    serde_json::from_str(raw).map_err(|e| corrupt::<C>(id, e))
}

fn encode<C: Component>(value: &C) -> Value {
    serde_json::to_value(value).expect("components serialize to JSON")
}

impl World {
    /// The component `C` on entity `id`, if it has one.
    pub fn get<C: Component>(&self, id: EntityId) -> Result<Option<C>, StoreError> {
        get_in(&self.conn, id)
    }

    /// Every entity carrying `C`, with its value, in id order.
    pub fn query<C: Component>(&self) -> Result<Vec<(EntityId, C)>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT entity_id, value FROM props WHERE key = ?1 ORDER BY entity_id")?;
        let rows = stmt.query_map([key::<C>()], |r| {
            Ok((r.get::<_, EntityId>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, s) = row?;
            out.push((id, decode_str(id, &s)?));
        }
        Ok(out)
    }

    /// Set the component `C` on `id` with a one-op engine patch.
    pub(crate) fn set<C: Component>(
        &mut self,
        id: EntityId,
        value: &C,
    ) -> Result<Applied, ApplyError> {
        self.apply_engine_patch(&Patch::new(vec![set_op(id, value)]))
    }
}

impl Entity {
    /// The component `C` among this entity's props, if present.
    pub fn get<C: Component>(&self) -> Result<Option<C>, StoreError> {
        self.props
            .get(key::<C>())
            .map(|v| decode(self.id, v))
            .transpose()
    }
}

/// The component `C` on entity `id`, read through `conn` (e.g. inside a
/// patch's transaction).
pub(crate) fn get_in<C: Component>(
    conn: &rusqlite::Connection,
    id: EntityId,
) -> Result<Option<C>, StoreError> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM props WHERE entity_id = ?1 AND key = ?2",
            rusqlite::params![id, key::<C>()],
            |r| r.get(0),
        )
        .optional()?;
    raw.map(|s| decode_str(id, &s)).transpose()
}

/// An op setting component `C` on `entity`, for multi-op engine patches.
pub(crate) fn set_op<C: Component>(entity: impl Into<EntityRef>, value: &C) -> Op {
    Op::SetProp {
        entity: entity.into(),
        key: key::<C>().to_owned(),
        value: encode(value),
    }
}

/// An op removing component `C` from `entity`.
pub(crate) fn remove_op<C: Component>(entity: impl Into<EntityRef>) -> Op {
    Op::RemoveProp {
        entity: entity.into(),
        key: key::<C>().to_owned(),
    }
}

/// A `(key, value)` prop entry for an `Op::Create`.
pub(crate) fn prop<C: Component>(value: &C) -> (String, Value) {
    (key::<C>().to_owned(), encode(value))
}
