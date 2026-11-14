//! Replay and branching: rebuild a world from its event log alone.
//!
//! Nothing here takes an LLM client. Replay re-applies the recorded patches
//! through the same validator that accepted them the first time and copies
//! every other event verbatim; recorded outcomes (dice rolls, judgments) are
//! read from the log, never recomputed.

use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::Value;

use super::{Entity, EntityId, EventKind, Patch, StoreError, World, apply_ops};

/// Why a log could not be replayed or forked.
#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    /// Replaying event `seq` did not reproduce what the log recorded.
    #[error("replay diverged at event {seq}: {reason}")]
    Diverged { seq: i64, reason: String },
    #[error("cannot fork at event {at}: the log ends at event {last}")]
    ForkPointOutOfRange { at: i64, last: i64 },
    #[error("fork target already holds a world")]
    TargetNotEmpty,
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl From<rusqlite::Error> for ReplayError {
    fn from(e: rusqlite::Error) -> Self {
        ReplayError::Store(StoreError::Sqlite(e))
    }
}

/// Everything that makes up the world's state, for comparing two stores.
#[derive(Debug, Clone, PartialEq)]
pub struct WorldState {
    /// Every entity, in id order.
    pub entities: Vec<Entity>,
    /// The id the next created entity will get.
    pub next_entity_id: EntityId,
}

/// An event row exactly as stored.
struct RawEvent {
    seq: i64,
    kind: String,
    payload: String,
    at: String,
}

impl World {
    /// Rebuild this world from its event log into a fresh in-memory store.
    /// The result has the same state and an identical log.
    pub fn replay_in_memory(&self) -> Result<World, ReplayError> {
        let target = World::open_in_memory()?;
        // A full replay of a branch is still that branch.
        let branch_point = self.forked_at()?;
        self.rebuild_into(self.last_seq()?, branch_point, target)
    }

    /// Branch this session at event `at`: a new in-memory world holding the
    /// log up to and including `at` and the state those events produce.
    /// `at = 0` gives an empty world. This world is left untouched.
    pub fn fork_in_memory(&self, at: i64) -> Result<World, ReplayError> {
        let target = World::open_in_memory()?;
        self.rebuild_into(at, Some(at), target)
    }

    /// Like [`fork_in_memory`](Self::fork_in_memory), but the branch is a
    /// new SQLite file at `path`, which must not already hold a world.
    pub fn fork(&self, at: i64, path: impl AsRef<Path>) -> Result<World, ReplayError> {
        let target = World::open(path)?;
        self.rebuild_into(at, Some(at), target)
    }

    /// The event this world was forked at, if it is a branch.
    pub fn forked_at(&self) -> Result<Option<i64>, StoreError> {
        let value: Option<String> = self
            .conn
            .query_row("SELECT value FROM meta WHERE key = 'forked_at'", [], |r| {
                r.get(0)
            })
            .optional()?;
        value
            .map(|v| {
                v.parse()
                    .map_err(|_| StoreError::Corrupt(format!("meta forked_at = {v:?}")))
            })
            .transpose()
    }

    /// A full snapshot of the world's state.
    pub fn state(&self) -> Result<WorldState, StoreError> {
        let ids: Vec<EntityId> = self
            .conn
            .prepare("SELECT id FROM entities ORDER BY id")?
            .query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        let mut entities = Vec::with_capacity(ids.len());
        for id in ids {
            let entity = self
                .entity(id)?
                .ok_or_else(|| StoreError::Corrupt(format!("entity {id} vanished")))?;
            entities.push(entity);
        }
        let last_id: Option<i64> = self
            .conn
            .query_row(
                "SELECT seq FROM sqlite_sequence WHERE name = 'entities'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        Ok(WorldState {
            entities,
            next_entity_id: last_id.unwrap_or(0) + 1,
        })
    }

    /// Sequence number of the last event, or 0 for an empty log.
    pub fn last_seq(&self) -> Result<i64, StoreError> {
        Ok(self
            .conn
            .query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |r| r.get(0))?)
    }

    /// Replay events up to `at` into the empty `target`, recording
    /// `branch_point` as where it was forked.
    fn rebuild_into(
        &self,
        at: i64,
        branch_point: Option<i64>,
        mut target: World,
    ) -> Result<World, ReplayError> {
        let last = self.last_seq()?;
        if !(0..=last).contains(&at) {
            return Err(ReplayError::ForkPointOutOfRange { at, last });
        }
        if !target.is_empty()? {
            return Err(ReplayError::TargetNotEmpty);
        }
        let events = self.raw_events(at)?;
        let tx = target.conn.transaction()?;
        for event in &events {
            replay_event(&tx, event)?;
        }
        if let Some(point) = branch_point {
            tx.execute(
                "INSERT INTO meta (key, value) VALUES ('forked_at', ?1)",
                [point.to_string()],
            )?;
        }
        tx.commit()?;
        Ok(target)
    }

    fn is_empty(&self) -> Result<bool, StoreError> {
        Ok(self.conn.query_row(
            "SELECT NOT EXISTS (SELECT 1 FROM events)
                AND NOT EXISTS (SELECT 1 FROM entities)
                AND NOT EXISTS (SELECT 1 FROM meta)",
            [],
            |r| r.get(0),
        )?)
    }

    /// Stored events with `seq <= up_to`, untouched by parsing.
    fn raw_events(&self, up_to: i64) -> Result<Vec<RawEvent>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT seq, kind, payload, at FROM events WHERE seq <= ?1 ORDER BY seq")?;
        let rows = stmt.query_map([up_to], |r| {
            Ok(RawEvent {
                seq: r.get(0)?,
                kind: r.get(1)?,
                payload: r.get(2)?,
                at: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

/// Re-apply one event's state changes (if any) and copy it into the log.
fn replay_event(tx: &Transaction, event: &RawEvent) -> Result<(), ReplayError> {
    let seq = event.seq;
    let kind = EventKind::parse(&event.kind).ok_or_else(|| {
        StoreError::Corrupt(format!("event {seq}: unknown kind {:?}", event.kind))
    })?;

    // Exhaustive on purpose: a new event kind that changes state must say
    // here how it replays.
    match kind {
        EventKind::Patch => replay_patch(tx, event)?,
        // Recorded for `:why` and the director; they change no state.
        // Rejected patches were rolled back, and rolls and judgments are
        // outcomes already applied through the patches that follow them.
        EventKind::Input
        | EventKind::PatchRejected
        | EventKind::ToolCall
        | EventKind::RuleChange
        | EventKind::Roll
        | EventKind::Judgment => {}
    }

    copy_event(tx, event)?;
    Ok(())
}

fn replay_patch(tx: &Transaction, event: &RawEvent) -> Result<(), ReplayError> {
    let seq = event.seq;
    let diverged = |reason: String| ReplayError::Diverged { seq, reason };

    let payload: Value = serde_json::from_str(&event.payload)
        .map_err(|e| StoreError::Corrupt(format!("event {seq}: bad payload: {e}")))?;
    let patch: Patch = serde_json::from_value(payload["patch"].clone())
        .map_err(|e| StoreError::Corrupt(format!("event {seq}: bad patch: {e}")))?;
    let created: Vec<EntityId> = serde_json::from_value(payload["created"].clone())
        .map_err(|e| StoreError::Corrupt(format!("event {seq}: bad created list: {e}")))?;
    let handles: BTreeMap<String, EntityId> = serde_json::from_value(payload["handles"].clone())
        .map_err(|e| StoreError::Corrupt(format!("event {seq}: bad handles: {e}")))?;

    // The whole replay is one transaction, so a rejection here aborts it
    // when the caller drops `tx`.
    let (got_created, got_handles) = apply_ops(tx, &patch)?
        .map_err(|r| diverged(format!("recorded patch is now rejected: {r}")))?;
    if got_created != created {
        return Err(diverged(format!(
            "patch created {got_created:?}, log recorded {created:?}"
        )));
    }
    if got_handles != handles {
        return Err(diverged(format!(
            "patch assigned handles {got_handles:?}, log recorded {handles:?}"
        )));
    }
    Ok(())
}

fn copy_event(conn: &Connection, event: &RawEvent) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO events (seq, kind, payload, at) VALUES (?1, ?2, ?3, ?4)",
        params![event.seq, event.kind, event.payload, event.at],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::Op;
    use serde_json::json;

    fn room(name: &str) -> Patch {
        Patch::new(vec![Op::Create {
            handle: Some("r".into()),
            name: name.into(),
            kind: "room".into(),
            location: None,
            props: Default::default(),
        }])
    }

    /// Rewrite an event's payload behind the append-only trigger, as a
    /// corrupted or hand-edited log would.
    fn tamper(w: &World, seq: i64, payload: Value) {
        w.conn
            .execute_batch("DROP TRIGGER events_no_update")
            .unwrap();
        w.conn
            .execute(
                "UPDATE events SET payload = ?1 WHERE seq = ?2",
                params![payload.to_string(), seq],
            )
            .unwrap();
    }

    #[test]
    fn patch_that_no_longer_validates_is_divergence() {
        let mut w = World::open_in_memory().unwrap();
        w.apply_patch(&room("Hall")).unwrap();
        let seq = w
            .apply_patch(&Patch::new(vec![Op::Rename {
                entity: 1.into(),
                name: "Great hall".into(),
            }]))
            .unwrap()
            .event_seq;
        tamper(
            &w,
            seq,
            json!({"patch": {"ops": [{"op": "rename", "entity": 99, "name": "x"}]},
                   "created": [], "handles": {}}),
        );
        match w.replay_in_memory() {
            Err(ReplayError::Diverged { seq: s, reason }) => {
                assert_eq!(s, seq);
                assert!(reason.contains("#99 does not exist"), "{reason}");
            }
            other => panic!("expected divergence, got {:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn mismatched_ids_are_divergence() {
        let mut w = World::open_in_memory().unwrap();
        let seq = w.apply_patch(&room("Hall")).unwrap().event_seq;
        tamper(
            &w,
            seq,
            json!({"patch": room("Hall"), "created": [5], "handles": {"r": 5}}),
        );
        assert!(matches!(
            w.replay_in_memory(),
            Err(ReplayError::Diverged { seq: 1, .. })
        ));
    }

    #[test]
    fn empty_world_replays_to_empty_world() {
        let w = World::open_in_memory().unwrap();
        let r = w.replay_in_memory().unwrap();
        assert_eq!(r.state().unwrap(), w.state().unwrap());
        assert_eq!(r.state().unwrap().next_entity_id, 1);
        assert_eq!(r.forked_at().unwrap(), None);
    }
}
