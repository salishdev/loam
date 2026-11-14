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

use super::{EntityId, ROOM_KIND, StoreError, World};

/// The relation joining two rooms: a passage from the subject room to the
/// object room, with props `{dir, label}`. A two-way passage is two rows.
/// The validator requires both ends to be rooms and a `dir` no other exit
/// from the subject uses. One exit leads from a room to another; relating
/// the pair again may change its label but not its `dir`.
pub const EXIT_REL: &str = "exit";

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

/// Why relating `subject -exit-> object` with `props` is invalid, if it is:
/// both ends must be rooms, `dir` a non-empty string, no other exit from
/// `subject` may already use that `dir`, and an existing exit from `subject`
/// to `object` may be relabelled but not given another `dir`.
pub(super) fn bad_exit(
    conn: &Connection,
    subject: EntityId,
    object: EntityId,
    props: &Map<String, Value>,
) -> rusqlite::Result<Option<String>> {
    for (end, id) in [("subject", subject), ("object", object)] {
        let kind: String =
            conn.query_row("SELECT kind FROM entities WHERE id = ?1", [id], |r| {
                r.get(0)
            })?;
        if kind != ROOM_KIND {
            return Ok(Some(format!(
                "an {EXIT_REL:?} joins two rooms, but its {end} #{id} is a {kind:?}"
            )));
        }
    }
    let Some(dir) = props
        .get("dir")
        .and_then(Value::as_str)
        .filter(|d| !d.trim().is_empty())
    else {
        return Ok(Some(format!(
            "an {EXIT_REL:?} needs props {{dir, label}} with a non-empty string dir"
        )));
    };
    for other in select(
        conn,
        &format!("{COLUMNS} WHERE subject = ?1 AND rel = ?2"),
        params![subject, EXIT_REL],
    )? {
        let other_dir = other.props.get("dir").and_then(Value::as_str);
        if other.object == object {
            // Relating the same pair again replaces the row, so a new dir
            // would silently rename the existing exit.
            if other_dir != Some(dir) {
                return Ok(Some(format!(
                    "#{subject} already has an exit {:?} to #{object}, and only one exit \
                     leads from a room to another; to rename it, unrelate it first",
                    other_dir.unwrap_or("")
                )));
            }
        } else if other_dir == Some(dir) {
            return Ok(Some(format!(
                "#{subject} already has an exit {dir:?} (to #{})",
                other.object
            )));
        }
    }
    Ok(None)
}
