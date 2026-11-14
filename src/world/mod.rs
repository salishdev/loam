//! The world store: entities in a containment tree, open key/value props,
//! validated patches and an append-only event log, all in SQLite.

mod log;
mod patch;
mod replay;

#[cfg(test)]
mod tests;

pub use log::{Event, EventKind};
pub use patch::{EntityId, EntityRef, Op, Patch};
pub use replay::{ReplayError, WorldState};

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Map, Value, json};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS entities (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    name     TEXT NOT NULL,
    kind     TEXT NOT NULL,
    location INTEGER REFERENCES entities(id)
);
CREATE INDEX IF NOT EXISTS entities_location ON entities(location);

CREATE TABLE IF NOT EXISTS props (
    entity_id INTEGER NOT NULL REFERENCES entities(id) ON DELETE CASCADE,
    key       TEXT NOT NULL,
    value     TEXT NOT NULL,
    PRIMARY KEY (entity_id, key)
);

CREATE TABLE IF NOT EXISTS events (
    seq     INTEGER PRIMARY KEY AUTOINCREMENT,
    kind    TEXT NOT NULL,
    payload TEXT NOT NULL,
    at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE TRIGGER IF NOT EXISTS events_no_update BEFORE UPDATE ON events
BEGIN SELECT RAISE(ABORT, 'event log is append-only'); END;
CREATE TRIGGER IF NOT EXISTS events_no_delete BEFORE DELETE ON events
BEGIN SELECT RAISE(ABORT, 'event log is append-only'); END;

CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
";

/// An entity: engine-owned core plus open props.
#[derive(Debug, Clone, PartialEq)]
pub struct Entity {
    pub id: EntityId,
    pub name: String,
    pub kind: String,
    /// Containing entity, or `None` at the top level.
    pub location: Option<EntityId>,
    pub props: Map<String, Value>,
}

/// Why a patch was rejected. Nothing in the patch was applied.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("op {op}: {reason}")]
pub struct Rejection {
    /// Index of the offending op in the patch.
    pub op: usize,
    /// Human-readable reason, suitable to hand back to the director.
    pub reason: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error("patch rejected: {0}")]
    Rejected(Rejection),
    #[error(transparent)]
    Storage(#[from] rusqlite::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("corrupt store: {0}")]
    Corrupt(String),
}

/// Result of an applied patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    /// Sequence number of the `patch` event that records it.
    pub event_seq: i64,
    /// Ids of entities created, in op order.
    pub created: Vec<EntityId>,
    /// Ids assigned to handles used in the patch.
    pub handles: BTreeMap<String, EntityId>,
}

/// Prop keys starting with this are engine-owned: director patches can't
/// set or remove them.
pub const ENGINE_PROP_PREFIX: &str = "_";

/// Engine-owned prop marking an entity whose contents are an inventory.
pub const CARRIER_PROP: &str = "_carrier";

/// Who is applying a patch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Authority {
    /// The director (LLM): engine-owned state is off limits.
    Director,
    /// An engine module: no ownership guard, only integrity checks.
    Engine,
}

/// The world store.
pub struct World {
    conn: Connection,
}

impl World {
    /// Open (or create) a world stored in a SQLite file.
    pub fn open(path: impl AsRef<Path>) -> Result<World, StoreError> {
        World::init(Connection::open(path)?)
    }

    /// A throwaway world held in memory.
    pub fn open_in_memory() -> Result<World, StoreError> {
        World::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<World, StoreError> {
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        conn.execute_batch(SCHEMA)?;
        Ok(World { conn })
    }

    /// Validate and apply a patch atomically, logging it as a `patch` event
    /// in the same transaction. A rejected patch changes nothing in the
    /// world and is logged as a `patch_rejected` event.
    ///
    /// This is the director's door: it refuses engine-owned changes (props
    /// whose key starts with `_`, and moves into or out of a carrier's
    /// inventory), which go through [`crate::inventory`] instead.
    pub fn apply_patch(&mut self, patch: &Patch) -> Result<Applied, ApplyError> {
        self.apply_patch_as(patch, Authority::Director)
    }

    /// Apply a patch without the engine-owned guard. Used by engine modules
    /// such as inventory; the `patch` event records `"by": "engine"` so
    /// replay knows to apply it the same way.
    pub(crate) fn apply_engine_patch(&mut self, patch: &Patch) -> Result<Applied, ApplyError> {
        self.apply_patch_as(patch, Authority::Engine)
    }

    fn apply_patch_as(&mut self, patch: &Patch, by: Authority) -> Result<Applied, ApplyError> {
        let tx = self.conn.transaction()?;
        match apply_ops(&tx, patch, by)? {
            Ok((created, handles)) => {
                let mut payload = json!({
                    "patch": patch,
                    "created": created,
                    "handles": handles,
                });
                if by == Authority::Engine {
                    payload["by"] = json!("engine");
                }
                let event_seq = insert_event(&tx, EventKind::Patch, &payload)?;
                tx.commit()?;
                Ok(Applied {
                    event_seq,
                    created,
                    handles,
                })
            }
            Err(rejection) => {
                tx.rollback()?;
                let mut payload = json!({
                    "patch": patch,
                    "op": rejection.op,
                    "reason": rejection.reason,
                });
                if by == Authority::Engine {
                    payload["by"] = json!("engine");
                }
                insert_event(&self.conn, EventKind::PatchRejected, &payload)?;
                Err(ApplyError::Rejected(rejection))
            }
        }
    }

    /// Append an event of any kind; returns its sequence number.
    pub fn log_event(&self, kind: EventKind, payload: &Value) -> Result<i64, StoreError> {
        Ok(insert_event(&self.conn, kind, payload)?)
    }

    /// All events in order.
    pub fn events(&self) -> Result<Vec<Event>, StoreError> {
        self.events_since(0)
    }

    /// Events with `seq > after`, in order.
    pub fn events_since(&self, after: i64) -> Result<Vec<Event>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT seq, kind, payload, at FROM events WHERE seq > ?1 ORDER BY seq")?;
        let rows = stmt.query_map([after], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        let mut events = Vec::new();
        for row in rows {
            let (seq, kind, payload, at) = row?;
            events.push(parse_event(seq, kind, payload, at)?);
        }
        Ok(events)
    }

    /// Look up one entity with its props.
    pub fn entity(&self, id: EntityId) -> Result<Option<Entity>, StoreError> {
        let core = self
            .conn
            .query_row(
                "SELECT name, kind, location FROM entities WHERE id = ?1",
                [id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<EntityId>>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((name, kind, location)) = core else {
            return Ok(None);
        };
        let mut stmt = self
            .conn
            .prepare("SELECT key, value FROM props WHERE entity_id = ?1 ORDER BY key")?;
        let mut props = Map::new();
        for row in stmt.query_map([id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })? {
            let (key, value) = row?;
            let value = serde_json::from_str(&value)
                .map_err(|e| StoreError::Corrupt(format!("entity {id} prop {key:?}: {e}")))?;
            props.insert(key, value);
        }
        Ok(Some(Entity {
            id,
            name,
            kind,
            location,
            props,
        }))
    }

    /// Ids of every entity of `kind`, in id order.
    pub fn entities_of_kind(&self, kind: &str) -> Result<Vec<EntityId>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM entities WHERE kind = ?1 ORDER BY id")?;
        let ids = stmt
            .query_map([kind], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        Ok(ids)
    }

    /// Up to `limit` events whose payload contains `text` (case-insensitive
    /// for ASCII), newest first. `kinds` restricts the search when non-empty.
    pub fn search_events(
        &self,
        text: &str,
        kinds: &[EventKind],
        limit: usize,
    ) -> Result<Vec<Event>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT seq, kind, payload, at FROM events
             WHERE instr(lower(payload), lower(?1)) > 0 ORDER BY seq DESC",
        )?;
        let mut rows = stmt.query([text])?;
        let mut found = Vec::new();
        while found.len() < limit
            && let Some(row) = rows.next()?
        {
            let event = parse_event(row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)?;
            if kinds.is_empty() || kinds.contains(&event.kind) {
                found.push(event);
            }
        }
        Ok(found)
    }

    /// Ids of entities directly inside `container` (`None` = top level).
    pub fn contents(&self, container: Option<EntityId>) -> Result<Vec<EntityId>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM entities WHERE location IS ?1 ORDER BY id")?;
        let ids = stmt
            .query_map([container], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        Ok(ids)
    }
}

fn parse_event(seq: i64, kind: String, payload: String, at: String) -> Result<Event, StoreError> {
    let kind = EventKind::parse(&kind)
        .ok_or_else(|| StoreError::Corrupt(format!("event {seq}: unknown kind {kind:?}")))?;
    let payload = serde_json::from_str(&payload)
        .map_err(|e| StoreError::Corrupt(format!("event {seq}: bad payload: {e}")))?;
    Ok(Event {
        seq,
        kind,
        payload,
        at,
    })
}

fn insert_event(conn: &Connection, kind: EventKind, payload: &Value) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO events (kind, payload) VALUES (?1, ?2)",
        params![kind.as_str(), payload.to_string()],
    )?;
    Ok(conn.last_insert_rowid())
}

type OpsOutcome = Result<(Vec<EntityId>, BTreeMap<String, EntityId>), Rejection>;

/// Apply ops in order inside `tx`. The outer `Result` is a storage failure;
/// the inner one is a validation verdict. The caller rolls back on rejection.
fn apply_ops(tx: &Transaction, patch: &Patch, by: Authority) -> rusqlite::Result<OpsOutcome> {
    let guarded = by == Authority::Director;
    let mut handles: BTreeMap<String, EntityId> = BTreeMap::new();
    let mut created = Vec::new();
    // Entities given a location by this patch, with the op that placed them.
    let mut placed: HashMap<EntityId, usize> = HashMap::new();

    for (i, op) in patch.ops.iter().enumerate() {
        let reject = |reason: String| Ok(Err(Rejection { op: i, reason }));
        macro_rules! resolve {
            ($r:expr) => {
                match resolve(tx, &handles, $r)? {
                    Ok(id) => id,
                    Err(reason) => return reject(reason),
                }
            };
        }

        match op {
            Op::Create {
                handle,
                name,
                kind,
                location,
                props,
            } => {
                if let Some(h) = handle
                    && handles.contains_key(h)
                {
                    return reject(format!("handle '{h}' is already used in this patch"));
                }
                let loc = match location {
                    Some(r) => Some(resolve!(r)),
                    None => None,
                };
                if guarded {
                    if let Some(key) = props.keys().find(|k| is_engine_prop(k)) {
                        return reject(engine_prop_reason(key));
                    }
                    if let Some(loc) = loc
                        && let Some(c) = carrier_at_or_above(tx, loc)?
                    {
                        return reject(format!(
                            "cannot create {name:?} inside #{c}'s inventory: \
                             create it elsewhere, then use give or transfer"
                        ));
                    }
                }
                tx.execute(
                    "INSERT INTO entities (name, kind, location) VALUES (?1, ?2, ?3)",
                    params![name, kind, loc],
                )?;
                let id = tx.last_insert_rowid();
                for (key, value) in props {
                    set_prop(tx, id, key, value)?;
                }
                if loc.is_some() {
                    placed.insert(id, i);
                }
                if let Some(h) = handle {
                    handles.insert(h.clone(), id);
                }
                created.push(id);
            }
            Op::Move { entity, to } => {
                let id = resolve!(entity);
                let dest = match to {
                    Some(r) => Some(resolve!(r)),
                    None => None,
                };
                if let Some(prev) = placed.get(&id) {
                    return reject(format!(
                        "{entity} would be in two places: op {prev} already placed it"
                    ));
                }
                if guarded {
                    if let Some(c) = carrier_above(tx, id)? {
                        return reject(format!(
                            "{entity} is in #{c}'s inventory, which only the inventory \
                             tools (drop, give, transfer) can change"
                        ));
                    }
                    if let Some(dest) = dest
                        && let Some(c) = carrier_at_or_above(tx, dest)?
                    {
                        return reject(format!(
                            "moving {entity} would put it in #{c}'s inventory, which only \
                             the inventory tools (take, give, transfer) can change"
                        ));
                    }
                }
                if let Some(dest) = dest
                    && let Some(reason) = cycle(tx, id, dest)?
                {
                    return reject(format!(
                        "moving {entity} into {}: {reason}",
                        to.as_ref().unwrap()
                    ));
                }
                tx.execute(
                    "UPDATE entities SET location = ?1 WHERE id = ?2",
                    params![dest, id],
                )?;
                placed.insert(id, i);
            }
            Op::SetProp { entity, key, value } => {
                let id = resolve!(entity);
                if guarded && is_engine_prop(key) {
                    return reject(engine_prop_reason(key));
                }
                set_prop(tx, id, key, value)?;
            }
            Op::RemoveProp { entity, key } => {
                let id = resolve!(entity);
                if guarded && is_engine_prop(key) {
                    return reject(engine_prop_reason(key));
                }
                tx.execute(
                    "DELETE FROM props WHERE entity_id = ?1 AND key = ?2",
                    params![id, key],
                )?;
            }
            Op::Rename { entity, name } => {
                let id = resolve!(entity);
                tx.execute(
                    "UPDATE entities SET name = ?1 WHERE id = ?2",
                    params![name, id],
                )?;
            }
            Op::Destroy { entity } => {
                let id = resolve!(entity);
                let inside: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM entities WHERE location = ?1",
                    [id],
                    |r| r.get(0),
                )?;
                if inside > 0 {
                    return reject(format!(
                        "cannot destroy {entity}: it still contains {inside} entit{}",
                        if inside == 1 { "y" } else { "ies" }
                    ));
                }
                tx.execute("DELETE FROM entities WHERE id = ?1", [id])?;
            }
        }
    }
    Ok(Ok((created, handles)))
}

/// Resolve a ref to an existing entity id, or explain why it's missing.
fn resolve(
    tx: &Transaction,
    handles: &BTreeMap<String, EntityId>,
    r: &EntityRef,
) -> rusqlite::Result<Result<EntityId, String>> {
    let id = match r {
        EntityRef::Id(id) => *id,
        EntityRef::Handle(h) => match handles.get(h) {
            Some(id) => *id,
            None => {
                return Ok(Err(format!(
                    "unknown handle '{h}': no earlier op in this patch creates it"
                )));
            }
        },
    };
    let exists = tx
        .query_row("SELECT 1 FROM entities WHERE id = ?1", [id], |_| Ok(()))
        .optional()?
        .is_some();
    Ok(if exists {
        Ok(id)
    } else {
        Err(format!("entity {r} does not exist"))
    })
}

/// If putting `id` inside `dest` would create a containment cycle, say why.
fn cycle(tx: &Transaction, id: EntityId, dest: EntityId) -> rusqlite::Result<Option<String>> {
    if id == dest {
        return Ok(Some("an entity cannot contain itself".into()));
    }
    let mut seen = HashSet::new();
    let mut cur = Some(dest);
    while let Some(c) = cur {
        if c == id {
            return Ok(Some(format!(
                "#{dest} is inside #{id}, so this would create a containment cycle"
            )));
        }
        if !seen.insert(c) {
            break; // existing cycle; the store never creates one, but don't loop
        }
        cur = tx.query_row("SELECT location FROM entities WHERE id = ?1", [c], |r| {
            r.get::<_, Option<EntityId>>(0)
        })?;
    }
    Ok(None)
}

fn is_engine_prop(key: &str) -> bool {
    key.starts_with(ENGINE_PROP_PREFIX)
}

fn engine_prop_reason(key: &str) -> String {
    format!(
        "prop {key:?} is engine-owned (keys starting with '{ENGINE_PROP_PREFIX}'); use the inventory tools"
    )
}

/// The nearest carrier strictly containing `id`, if any.
fn carrier_above(tx: &Transaction, id: EntityId) -> rusqlite::Result<Option<EntityId>> {
    let loc = tx.query_row("SELECT location FROM entities WHERE id = ?1", [id], |r| {
        r.get::<_, Option<EntityId>>(0)
    })?;
    match loc {
        Some(loc) => carrier_at_or_above(tx, loc),
        None => Ok(None),
    }
}

/// `id` itself if it is a carrier, else the nearest carrier containing it.
fn carrier_at_or_above(tx: &Transaction, id: EntityId) -> rusqlite::Result<Option<EntityId>> {
    let mut seen = HashSet::new();
    let mut cur = Some(id);
    while let Some(c) = cur {
        if !seen.insert(c) {
            break;
        }
        let is_carrier = tx
            .query_row(
                "SELECT 1 FROM props WHERE entity_id = ?1 AND key = ?2 AND value = 'true'",
                params![c, CARRIER_PROP],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if is_carrier {
            return Ok(Some(c));
        }
        cur = tx.query_row("SELECT location FROM entities WHERE id = ?1", [c], |r| {
            r.get::<_, Option<EntityId>>(0)
        })?;
    }
    Ok(None)
}

fn set_prop(tx: &Transaction, id: EntityId, key: &str, value: &Value) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO props (entity_id, key, value) VALUES (?1, ?2, ?3)
         ON CONFLICT (entity_id, key) DO UPDATE SET value = excluded.value",
        params![id, key, value.to_string()],
    )?;
    Ok(())
}
