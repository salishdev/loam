//! The quest judge: one Haiku call per active quest a turn touched.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

use serde::Deserialize;
use serde_json::{Value, json};

use super::{CONCERNS_REL, Quest, Status, quests, resolve};
use crate::llm::{LlmClient, Message, Model, Request, ToolDefinition};
use crate::world::{Cascaded, EntityId, EventKind, StoreError, World};

/// The tool the judge answers with.
pub const JUDGE_TOOL: &str = "record_judgment";

const JUDGE_SYSTEM: &str = "You judge quests in Loam, a text adventure. You get one quest's \
recorded goal and what changed in the world this turn. Decide against the recorded goal \
only: complete if it is now met, failed if it can no longer be met, otherwise ongoing. The \
entity state shown is the truth; narration may embellish. Reply with one record_judgment \
call and nothing else.";

/// Longest chunk of turn context (applied ops, narration) shown to the judge.
const CONTEXT_CHARS: usize = 4000;

/// The judge's ruling on one quest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Complete,
    Failed,
    Ongoing,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Complete => "complete",
            Verdict::Failed => "failed",
            Verdict::Ongoing => "ongoing",
        }
    }
}

/// One logged judgment.
#[derive(Debug, Clone, PartialEq)]
pub struct Judgment {
    pub quest: EntityId,
    pub title: String,
    pub verdict: Verdict,
    pub reason: String,
    /// Sequence number of the `judgment` event.
    pub event_seq: i64,
}

#[derive(Deserialize)]
struct JudgeInput {
    verdict: Verdict,
    reason: String,
}

fn judge_tool() -> ToolDefinition {
    ToolDefinition::new(
        JUDGE_TOOL,
        "Record your ruling on the quest.",
        json!({
            "type": "object",
            "properties": {
                "verdict": {"type": "string", "enum": ["complete", "failed", "ongoing"]},
                "reason": {"type": "string", "description": "One sentence citing the state that decides it."}
            },
            "required": ["verdict", "reason"]
        }),
    )
}

/// Judge every active quest whose concerned entities were touched by the
/// patches applied after event `turn_start`, and only those. `input` and
/// `narration` describe the turn.
///
/// Each judgment is logged as a `judgment` event; a complete or failed
/// verdict then resolves the quest through an engine patch. A judge call
/// that fails, or answers without a usable verdict, is logged as
/// `ongoing` with an `error` and changes nothing: the quest is judged
/// again the next time a turn touches it.
pub fn judge_turn(
    world: &mut World,
    client: &dyn LlmClient,
    turn_start: i64,
    input: Option<&str>,
    narration: &str,
) -> Result<Vec<Judgment>, StoreError> {
    let touched = world.changed_since(turn_start)?;
    if touched.is_empty() {
        return Ok(Vec::new());
    }
    let mut lost = lost_concerns(world, turn_start)?;
    let due: Vec<Quest> = quests(world)?
        .into_iter()
        .map(|mut q| {
            // A concerned entity destroyed this turn took its relation with
            // it; the judge still needs to see it gone.
            q.concerns.extend(lost.remove(&q.id).unwrap_or_default());
            q
        })
        .filter(|q| q.status == Status::Active && q.concerns.iter().any(|c| touched.contains(c)))
        .collect();
    if due.is_empty() {
        return Ok(Vec::new());
    }
    let changes = applied_ops(world, turn_start)?;
    let mut judgments = Vec::new();
    for q in due {
        let prompt = judge_prompt(world, &q, &touched, input, &changes, narration)?;
        let request = Request::new(Model::Haiku, vec![Message::user(prompt)])
            .system(JUDGE_SYSTEM)
            .role("judge")
            .max_tokens(2_000)
            .tools(vec![judge_tool()]);
        let (ruling, error) = match client.complete(&request) {
            Ok(response) => match response.tool_uses().find(|t| t.name == JUDGE_TOOL) {
                Some(call) => match serde_json::from_value::<JudgeInput>(call.input.clone()) {
                    Ok(r) => (Some(r), None),
                    Err(e) => (None, Some(format!("bad verdict: {e}"))),
                },
                None => (None, Some("the judge gave no verdict".to_owned())),
            },
            Err(e) => (None, Some(format!("judge call failed: {e}"))),
        };
        let (verdict, reason) = match ruling {
            Some(r) => (r.verdict, r.reason.trim().to_owned()),
            None => (Verdict::Ongoing, String::new()),
        };
        let mut payload = json!({
            "quest": q.id,
            "title": q.title,
            "goal": q.goal,
            "verdict": verdict.as_str(),
            "reason": reason,
            "touched": q.concerns.iter().filter(|c| touched.contains(c)).collect::<Vec<_>>(),
            "model": Model::Haiku.id(),
        });
        if let Some(error) = &error {
            payload["error"] = json!(error);
        }
        let event_seq = world.log_event(EventKind::Judgment, &payload)?;
        let status = match verdict {
            Verdict::Complete => Some(Status::Complete),
            Verdict::Failed => Some(Status::Failed),
            Verdict::Ongoing => None,
        };
        if let Some(status) = status {
            match resolve(world, q.id, status, &reason) {
                Ok(()) => {}
                Err(super::QuestError::Store(e)) => return Err(e),
                // A quest entity always accepts its own status props.
                Err(super::QuestError::Refused(r)) => {
                    return Err(StoreError::Corrupt(format!("quest #{}: {r}", q.id)));
                }
            }
        }
        judgments.push(Judgment {
            quest: q.id,
            title: q.title,
            verdict,
            reason,
            event_seq,
        });
    }
    Ok(judgments)
}

/// Concerns that destroys after event `after` cascaded away, by quest.
fn lost_concerns(
    world: &World,
    after: i64,
) -> Result<BTreeMap<EntityId, Vec<EntityId>>, StoreError> {
    let mut lost: BTreeMap<EntityId, Vec<EntityId>> = BTreeMap::new();
    for e in world.events_since(after)? {
        let Some(cascaded) = e
            .payload
            .get("cascaded")
            .filter(|_| e.kind == EventKind::Patch)
        else {
            continue;
        };
        let cascaded: Vec<Cascaded> = serde_json::from_value(cascaded.clone()).map_err(|err| {
            StoreError::Corrupt(format!("event {}: bad cascaded list: {err}", e.seq))
        })?;
        for c in cascaded {
            if c.relation.rel == CONCERNS_REL {
                lost.entry(c.relation.subject)
                    .or_default()
                    .push(c.relation.object);
            }
        }
    }
    Ok(lost)
}

/// The ops of every patch applied after `after`, one JSON line each.
fn applied_ops(world: &World, after: i64) -> Result<String, StoreError> {
    let mut out = String::new();
    for e in world.events_since(after)? {
        if e.kind == EventKind::Patch {
            writeln!(out, "{}", e.payload["patch"]["ops"]).unwrap();
        }
    }
    Ok(clip(&out, CONTEXT_CHARS))
}

fn judge_prompt(
    world: &World,
    q: &Quest,
    touched: &BTreeSet<EntityId>,
    input: Option<&str>,
    changes: &str,
    narration: &str,
) -> Result<String, StoreError> {
    let mut out = String::new();
    writeln!(out, "Quest: {}", q.title).unwrap();
    writeln!(out, "How it began: {}", q.hook).unwrap();
    writeln!(out, "Recorded goal: {}", q.goal.as_deref().unwrap_or("")).unwrap();
    for d in &q.developments {
        writeln!(out, "Development: {d}").unwrap();
    }
    writeln!(out, "\nEntities the goal concerns, as they are now:").unwrap();
    for id in &q.concerns {
        let mark = if touched.contains(id) {
            " (changed this turn)"
        } else {
            ""
        };
        match world.entity(*id)? {
            Some(e) => {
                let place = match e.location {
                    Some(loc) => match world.entity(loc)? {
                        Some(l) => format!("in #{} {}", l.id, l.name),
                        None => format!("in #{loc}"),
                    },
                    None => "nowhere".to_owned(),
                };
                writeln!(
                    out,
                    "- #{} {} [{}] {place}{mark} {}",
                    e.id,
                    e.name,
                    e.kind,
                    Value::Object(e.props)
                )
                .unwrap();
            }
            None => writeln!(out, "- #{id}: destroyed{mark}").unwrap(),
        }
    }
    writeln!(out, "\nThis turn").unwrap();
    if let Some(input) = input {
        writeln!(out, "Player input: {input}").unwrap();
    }
    writeln!(out, "Applied changes (patch ops):\n{changes}").unwrap();
    writeln!(out, "Narration: {}", clip(narration, CONTEXT_CHARS)).unwrap();
    Ok(out)
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_owned()
    } else {
        let cut: String = text.chars().take(max).collect();
        format!("{cut}…")
    }
}
