//! Relations: directional `(subject, rel, object)` facts between entities,
//! each with its own props. At most one relation of a given name joins an
//! ordered pair; a symmetric fact is two rows.
//!
//! They change only through [`Op::Relate`](super::Op::Relate) and
//! [`Op::Unrelate`](super::Op::Unrelate), and destroying either end removes
//! them in the same patch (logged as `cascaded`), so the event log, replay
//! and forking cover them like everything else.

use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::{EntityId, StoreError, World};

/// One relation row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Relation {
    pub subject: EntityId,
    pub rel: String,
    pub object: EntityId,
    #[serde(default)]
    pub props: Map<String, Value>,
}

const COLUMNS: &str = "SELECT subject, rel, object, props FROM relations";

impl World {
    /// Relations `id` is the subject of, optionally only those named `rel`,
    /// ordered by name then object.
    pub fn relations_from(
        &self,
        id: EntityId,
        rel: Option<&str>,
    ) -> Result<Vec<Relation>, StoreError> {
        Ok(select(
            &self.conn,
            &format!(
                "{COLUMNS} WHERE subject = ?1 AND (?2 IS NULL OR rel = ?2) ORDER BY rel, object"
            ),
            params![id, rel],
        )?)
    }

    /// Relations `id` is the object of, optionally only those named `rel`,
    /// ordered by name then subject.
    pub fn relations_to(
        &self,
        id: EntityId,
        rel: Option<&str>,
    ) -> Result<Vec<Relation>, StoreError> {
        Ok(select(
            &self.conn,
            &format!(
                "{COLUMNS} WHERE object = ?1 AND (?2 IS NULL OR rel = ?2) ORDER BY rel, subject"
            ),
            params![id, rel],
        )?)
    }

    /// Every relation, in key order.
    pub fn relations(&self) -> Result<Vec<Relation>, StoreError> {
        Ok(select(
            &self.conn,
            &format!("{COLUMNS} ORDER BY subject, rel, object"),
            [],
        )?)
    }
}

/// Every relation `id` is subject or object of, in key order.
pub(super) fn touching(conn: &Connection, id: EntityId) -> rusqlite::Result<Vec<Relation>> {
    select(
        conn,
        &format!("{COLUMNS} WHERE subject = ?1 OR object = ?1 ORDER BY subject, rel, object"),
        [id],
    )
}

fn select(
    conn: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> rusqlite::Result<Vec<Relation>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params, |r| {
        let raw: String = r.get(3)?;
        let props = serde_json::from_str(&raw).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e))
        })?;
        Ok(Relation {
            subject: r.get(0)?,
            rel: r.get(1)?,
            object: r.get(2)?,
            props,
        })
    })?;
    rows.collect()
}
