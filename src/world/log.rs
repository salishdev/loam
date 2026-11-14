//! Append-only event log: the source of truth for replay and `:why`.

use serde_json::Value;

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
}

impl EventKind {
    pub const ALL: [EventKind; 7] = [
        EventKind::Input,
        EventKind::Patch,
        EventKind::PatchRejected,
        EventKind::ToolCall,
        EventKind::RuleChange,
        EventKind::Roll,
        EventKind::Judgment,
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
