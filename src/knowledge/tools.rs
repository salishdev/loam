//! The director's `learn` tool: discoveries staged during a turn and
//! committed once the turn's narration exists.
//!
//! A `learn` call is checked when made, so a refusal reaches the director,
//! but writes nothing. [`commit_turn`] reads the turn's successful calls
//! back from the log after the narration is logged and writes each through
//! [`learn`](super::learn), which checks every citation again. A turn that
//! fails or ends without narration commits nothing.

use serde_json::{Value, json};

use super::{Category, Discovery, FactKind, Found, check, entry, find, learn, supported};
use crate::llm::{ToolDefinition, ToolOutput, ToolUse};
use crate::world::{EventKind, StoreError, World};

/// The tool's name.
pub const LEARN: &str = "learn";
/// The read-only tool that shows an entry's numbered facts.
pub const RECALL: &str = "recall";

/// The `learn` tool, offered on director turns (never in ticks).
pub fn tool_definitions() -> Vec<ToolDefinition> {
    let category = json!({
        "type": "string",
        "enum": Category::ALL.map(|c| c.label()),
    });
    vec![
        ToolDefinition::new(
            LEARN,
            "Record something the player learns this turn about one person, place, faction, \
         notable item, lore subject or mechanic, for their compendium. Record only what your \
         narration states: seen directly (an observation) or told by someone (a claim, with \
         claimed_by; a rumour stays a claim, never fact). Add nothing the narration doesn't \
         say. Each fact quotes an excerpt: words your final narration will include word for \
         word (or, with source, words of an earlier narration event). A fact whose excerpt \
         isn't narrated is dropped. Places the player enters are remembered without this; to add \
         to one, use entry (a refusal for its name gives the id). A \
         subject merely mentioned needs no world entity: don't create one to learn about it. \
         Name and category start an entry; if one of that name is already known, the tool \
         refuses and lists it with its id: then add to it with entry, or pass distinct: true \
         for a different subject that shares the name. When someone or something known only by \
         mention turns up in person, link the entry to its world entity: entry plus subject (facts \
         may then be empty); never link by a shared name alone. A fact that corrects earlier ones \
         lists their numbers (from recall) in supersedes: the earlier facts stay, attributed, as \
         superseded. People and notable items the narration names around the player are \
         remembered as seen without this.",
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
                                "claimant": {"type": "integer", "description": "The entry for whoever said it, when known."},
                                "supersedes": {"type": "array", "items": {"type": "integer"}, "description": "Numbers of the entry's earlier facts this one corrects (see recall)."}
                            },
                            "required": ["text", "excerpt"]
                        }
                    }
                },
            }),
        ),
        ToolDefinition::new(
            RECALL,
            "Look up what the player's compendium holds about a name or #entry: each matching \
         entry's id, whether it is linked to a world entity, and its facts numbered for \
         learn's supersedes. Reads only; changes nothing.",
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "A name, part of one, or #entry."}
                },
                "required": ["query"]
            }),
        ),
    ]
}

/// Handle a `learn` call made in the turn that began after event
/// `turn_start`, or `None` for other tools.
pub fn handle_tool(
    world: &World,
    turn_start: i64,
    call: &ToolUse,
) -> Option<Result<ToolOutput, StoreError>> {
    match call.name.as_str() {
        LEARN => Some(stage(world, turn_start, &call.input)),
        RECALL => Some(recall(world, &call.input)),
        _ => None,
    }
}

fn recall(world: &World, input: &Value) -> Result<ToolOutput, StoreError> {
    let Some(query) = input["query"]
        .as_str()
        .map(str::trim)
        .filter(|q| !q.is_empty())
    else {
        return Ok(ToolOutput::Err(format!("{RECALL} needs a query")));
    };
    let found = match find(world, query)? {
        Found::None => return Ok(ToolOutput::Ok(format!("No entry matches {query:?}."))),
        Found::One(e) => vec![e],
        Found::Many(many) => many,
    };
    let mut out = Vec::new();
    for e in found {
        let superseded = e.superseded();
        let link = if e.linked {
            "linked to its world entity"
        } else {
            "not linked to a world entity"
        };
        let mut lines = vec![format!(
            "#{} {} ({}), {link}, seen {} time(s)",
            e.id,
            e.name,
            e.category.label(),
            e.encounters
        )];
        if let Some(p) = e.seen_at.map(|p| entry(world, p)).transpose()?.flatten() {
            lines.push(format!("  last seen at {}", p.name));
        }
        for (i, f) in e.facts.iter().enumerate() {
            let how = match (&f.kind, &f.claimed_by) {
                (FactKind::Claimed, Some(by)) => format!("claimed by {by}"),
                (FactKind::Claimed, None) => "claimed".to_owned(),
                (FactKind::Observed, _) => "observed".to_owned(),
            };
            let gone = if superseded.contains(&(i + 1)) {
                " [superseded]"
            } else {
                ""
            };
            lines.push(format!("  {}. ({how}) {}{gone}", i + 1, f.text));
        }
        out.push(lines.join("\n"));
    }
    Ok(ToolOutput::Ok(out.join("\n")))
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
