//! Append-only event log: the source of truth for replay and `:why`.

use serde_json::{Map, Value};

use super::{ENGINE_PROP_PREFIX, EntityId, EntityRef, Op, Patch, StoreError};

fn is_engine_owned(rel: &str) -> bool {
    rel.starts_with(ENGINE_PROP_PREFIX)
}

/// What kind of thing an event records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventKind {
    /// Raw player input.
    Input,
    /// A patch that was applied.
    Patch,
    /// A patch that was rejected, with the reason.
    PatchRejected,
    /// An LLM tool call and its result.
    ToolCall,
    /// A rule minted, amended or retired.
    RuleChange,
    /// A dice roll, with its seed.
    Roll,
    /// An LLM judgment (e.g. quest completion).
    Judgment,
    /// Prose the narrator showed the player for a turn.
    Narration,
    /// A world tick: off-screen arcs advanced between turns. Its patches
    /// are logged as ordinary `patch` events; this records the summary.
    Tick,
    /// The world clock moved on: how far, who decided and the new time.
    /// The clock itself changes through the `patch` before it.
    Time,
}

impl EventKind {
    pub const ALL: [EventKind; 10] = [
        EventKind::Input,
        EventKind::Patch,
        EventKind::PatchRejected,
        EventKind::ToolCall,
        EventKind::RuleChange,
        EventKind::Roll,
        EventKind::Judgment,
        EventKind::Narration,
        EventKind::Tick,
        EventKind::Time,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::Input => "input",
            EventKind::Patch => "patch",
            EventKind::PatchRejected => "patch_rejected",
            EventKind::ToolCall => "tool_call",
            EventKind::RuleChange => "rule_change",
            EventKind::Roll => "roll",
            EventKind::Judgment => "judgment",
            EventKind::Narration => "narration",
            EventKind::Tick => "tick",
            EventKind::Time => "time",
        }
    }

    pub fn parse(s: &str) -> Option<EventKind> {
        EventKind::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

/// One row of the event log.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    /// Monotonic sequence number, starting at 1.
    pub seq: i64,
    pub kind: EventKind,
    pub payload: Value,
    /// UTC timestamp, RFC 3339 with milliseconds.
    pub at: String,
}

impl Event {
    /// For a `patch` or `patch_rejected` event, each op with the entities it
    /// touched: created (and where), moved (and where to), given or stripped
    /// of props, renamed or destroyed, or either end of a relation made or
    /// removed (only the subject, for an engine-owned relation such as a
    /// quest's concerns). Relations a destroy cascaded are in the payload's
    /// `cascaded` list, not here. A move's origin isn't logged, so it
    /// doesn't count. A rejected patch created nothing, so only ops naming
    /// existing ids count there. Other kinds of event touch nothing.
    pub fn patch_touches(&self) -> Result<Vec<(Op, Vec<EntityId>)>, StoreError> {
        if !matches!(self.kind, EventKind::Patch | EventKind::PatchRejected) {
            return Ok(Vec::new());
        }
        let corrupt = |what: String| StoreError::Corrupt(format!("event {}: {what}", self.seq));
        let patch: Patch = serde_json::from_value(self.payload["patch"].clone())
            .map_err(|e| corrupt(format!("bad patch: {e}")))?;
        let created: Vec<EntityId> = match self.payload.get("created") {
            Some(c) => serde_json::from_value(c.clone())
                .map_err(|e| corrupt(format!("bad created list: {e}")))?,
            None => Vec::new(),
        };
        let handles: Map<String, Value> = self.payload["handles"]
            .as_object()
            .cloned()
            .unwrap_or_default();
        let id_of = |r: &EntityRef| match r {
            EntityRef::Id(id) => Some(*id),
            EntityRef::Handle(h) => handles.get(h).and_then(Value::as_i64),
        };
        let mut created = created.into_iter();
        Ok(patch
            .ops
            .into_iter()
            .map(|op| {
                let ids: Vec<EntityId> = match &op {
                    Op::Create { location, .. } => created
                        .next()
                        .into_iter()
                        .chain(location.as_ref().and_then(id_of))
                        .collect(),
                    Op::Move { entity, to } => id_of(entity)
                        .into_iter()
                        .chain(to.as_ref().and_then(id_of))
                        .collect(),
                    Op::SetProp { entity, .. }
                    | Op::RemoveProp { entity, .. }
                    | Op::Rename { entity, .. }
                    | Op::Destroy { entity } => id_of(entity).into_iter().collect(),
                    // An engine-owned relation (a quest's concerns, a
                    // rule's pins) is the subject's bookkeeping: it doesn't
                    // change the object.
                    Op::Relate {
                        subject,
                        rel,
                        object,
                        ..
                    }
                    | Op::Unrelate {
                        subject,
                        rel,
                        object,
                        ..
                    } if is_engine_owned(rel) => id_of(subject).into_iter().collect(),
                    Op::Relate {
                        subject, object, ..
                    }
                    | Op::Unrelate {
                        subject, object, ..
                    } => id_of(subject).into_iter().chain(id_of(object)).collect(),
                };
                (op, ids)
            })
            .collect())
    }
}
