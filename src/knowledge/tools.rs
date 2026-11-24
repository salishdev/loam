//! The director's `learn` tool: discoveries staged during a turn and
//! committed once the turn's narration exists.
//!
//! A `learn` call is checked when made, so a refusal reaches the director,
//! but writes nothing. [`commit_turn`] reads the turn's successful calls
//! back from the log after the narration is logged and writes each through
//! [`learn`](super::learn), which checks every citation again. A turn that
//! fails or ends without narration commits nothing.

use serde_json::{Value, json};

use super::{Category, Discovery, check, learn, supported};
use crate::llm::{ToolDefinition, ToolOutput, ToolUse};
use crate::world::{EventKind, StoreError, World};

/// The tool's name.
pub const LEARN: &str = "learn";

/// The `learn` tool, offered on director turns (never in ticks).
pub fn tool_definitions() -> Vec<ToolDefinition> {
    let category = json!({
        "type": "string",
        "enum": Category::ALL.map(|c| c.label()),
    });
    vec![ToolDefinition::new(
        LEARN,
        "Record something the player learns this turn about one person, place, faction, \
         notable item, lore subject or mechanic, for their compendium. Record only what your \
         narration states: seen directly (an observation) or told by someone (a claim, with \
         claimed_by; a rumour stays a claim, never fact). Add nothing the narration doesn't \
         say. Each fact quotes an excerpt: words your final narration will include word for \
         word (or, with source, words of an earlier narration event). A fact whose excerpt \
         isn't narrated is dropped. Places the player enters are remembered without this; to add \
         to one, give its room as subject. A \
         subject merely mentioned needs no world entity: don't create one to learn about it. \
         Name and category start an entry; if one of that name is already known, the tool \
         refuses and lists it with its id: then add to it with entry, or pass distinct: true \
         for a different subject that shares the name.",
        json!({
            "type": "object",
            "properties": {
                "entry": {"type": "integer", "description": "A known entry to add to."},
                "name": {"type": "string", "description": "A new entry's name, as the player knows it."},
                "category": category,
                "distinct": {"type": "boolean", "description": "A new entry even though one of this name and category is known."},
                "subject": {"type": "integer", "description": "The world entity this is about, when it exists."},
                "facts": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": {
                            "text": {"type": "string", "description": "What was learned, one sentence."},
                            "excerpt": {"type": "string", "description": "At least three words of narration, quoted exactly."},
                            "source": {"type": "integer", "description": "An earlier narration event the excerpt is from; omit for this turn's narration."},
                            "claimed_by": {"type": "string", "description": "Who said it, for a claim or rumour."},
                            "claimant": {"type": "integer", "description": "The entry for whoever said it, when known."}
                        },
                        "required": ["text", "excerpt"]
                    }
                }
            },
            "required": ["facts"]
        }),
    )]
}

/// Handle a `learn` call made in the turn that began after event
/// `turn_start`, or `None` for other tools.
pub fn handle_tool(
    world: &World,
    turn_start: i64,
    call: &ToolUse,
) -> Option<Result<ToolOutput, StoreError>> {
    if call.name != LEARN {
        return None;
    }
    Some(stage(world, turn_start, &call.input))
}

fn stage(world: &World, turn_start: i64, input: &Value) -> Result<ToolOutput, StoreError> {
    let d: Discovery = match serde_json::from_value(input.clone()) {
        Ok(d) => d,
        Err(e) => return Ok(ToolOutput::Err(format!("bad input for {LEARN}: {e}"))),
    };
    // Only this turn's calls: a failed earlier attempt's never commit.
    let pending = staged_since(world, turn_start)?;
    if let Err(why) = check(world, &d, None, &pending)? {
        return Ok(ToolOutput::Err(why));
    }
    let entry = d.entry.map_or(json!("new"), |id| json!(id));
    Ok(ToolOutput::Ok(
        json!({
            "staged": true,
            "entry": entry,
            "note": "recorded when the turn ends, for facts whose excerpt the narration includes word for word",
        })
        .to_string(),
    ))
}

/// The turn's successful `learn` calls after `after`, in order.
fn staged_since(world: &World, after: i64) -> Result<Vec<Discovery>, StoreError> {
    Ok(world
        .events_since(after)?
        .into_iter()
        .filter(|e| {
            e.kind == EventKind::ToolCall
                && e.payload["name"] == LEARN
                && e.payload["is_error"] != json!(true)
        })
        .filter_map(|e| serde_json::from_value(e.payload["input"].clone()).ok())
        .collect())
}

/// Write the discoveries staged since event `after`, citing `narration`
/// (the turn's narration event) where they cite this turn. A fact whose
/// excerpt isn't narrated is dropped; each discovery is written on its own,
/// so a refused one leaves the others, and everything known before, as it
/// was.
pub(crate) fn commit_turn(world: &mut World, after: i64, narration: i64) -> Result<(), StoreError> {
    for d in staged_since(world, after)? {
        if let Some(d) = supported(world, &d, narration)? {
            let _ = learn(world, &d, Some(narration))?;
        }
    }
    Ok(())
}
