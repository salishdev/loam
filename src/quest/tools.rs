//! The quest tools the director calls.

use serde::Deserialize;
use serde_json::{Value, json};

use super::{Quest, QuestError, escalate, reveal_goal, spawn};
use crate::llm::{ToolDefinition, ToolOutput, ToolUse};
use crate::world::{EntityId, StoreError, World};

fn ids(what: &str) -> Value {
    json!({"type": "array", "items": {"type": "integer"}, "description": what})
}

/// Schemas for the director's quest tools.
pub fn tool_definitions() -> Vec<ToolDefinition> {
    let quest = json!({"type": "integer", "description": "The quest's id."});
    vec![
        ToolDefinition::new(
            "spawn_quest",
            "Start a quest when play suggests one: an NPC's plea, a discovered secret, a \
             threat. Its goal comes later, with reveal_goal, once it is clear what would \
             count as done. Quests should give the player reasons to act.",
            json!({
                "type": "object",
                "properties": {
                    "title": {"type": "string"},
                    "hook": {"type": "string", "description": "How the quest surfaced in play, one sentence."},
                    "concerns": ids("Entity ids already involved, if any.")
                },
                "required": ["title", "hook"]
            }),
        ),
        ToolDefinition::new(
            "reveal_goal",
            "Record an open quest's goal once it has revealed itself to the player, with \
             the entities it concerns. The goal is fixed from then on and a judge checks it \
             on turns that change one of those entities, so make it concrete and checkable \
             from world state, and list every entity (item, NPC, place) whose change could \
             complete or fail it. You never mark a quest complete yourself.",
            json!({
                "type": "object",
                "properties": {
                    "quest": quest,
                    "goal": {"type": "string", "description": "What must be true for the quest to be complete."},
                    "concerns": ids("Entity ids the goal concerns (at least one).")
                },
                "required": ["quest", "goal", "concerns"]
            }),
        ),
        ToolDefinition::new(
            "escalate_quest",
            "Raise the stakes on an unresolved quest: record a development (a complication, \
             a deadline, a rival) and any newly involved entities. The goal never changes.",
            json!({
                "type": "object",
                "properties": {
                    "quest": quest,
                    "development": {"type": "string"},
                    "concerns": ids("More entity ids the quest now concerns.")
                },
                "required": ["quest", "development"]
            }),
        ),
    ]
}

#[derive(Deserialize)]
struct SpawnInput {
    title: String,
    hook: String,
    #[serde(default)]
    concerns: Vec<EntityId>,
}

#[derive(Deserialize)]
struct RevealInput {
    quest: EntityId,
    goal: String,
    concerns: Vec<EntityId>,
}

#[derive(Deserialize)]
struct EscalateInput {
    quest: EntityId,
    development: String,
    #[serde(default)]
    concerns: Vec<EntityId>,
}

/// Run a quest tool call; `None` if `call` isn't one. `Err` only for
/// storage failures.
pub fn handle_tool(world: &mut World, call: &ToolUse) -> Option<Result<ToolOutput, StoreError>> {
    fn parse<T: for<'de> Deserialize<'de>>(call: &ToolUse) -> Result<T, QuestError> {
        serde_json::from_value(call.input.clone())
            .map_err(|e| QuestError::Refused(format!("bad input for {}: {e}", call.name)))
    }
    let result = match call.name.as_str() {
        "spawn_quest" => parse::<SpawnInput>(call).and_then(|i| {
            spawn(world, &i.title, &i.hook, &i.concerns)
                .map(|id| json!({"quest": id, "status": "open"}))
        }),
        "reveal_goal" => parse::<RevealInput>(call)
            .and_then(|i| reveal_goal(world, i.quest, &i.goal, &i.concerns).map(|q| view(&q))),
        "escalate_quest" => parse::<EscalateInput>(call)
            .and_then(|i| escalate(world, i.quest, &i.development, &i.concerns).map(|q| view(&q))),
        _ => return None,
    };
    Some(match result {
        Ok(v) => Ok(ToolOutput::Ok(v.to_string())),
        Err(QuestError::Refused(r)) => Ok(ToolOutput::Err(r)),
        Err(QuestError::Store(e)) => Err(e),
    })
}

fn view(q: &Quest) -> Value {
    json!({
        "quest": q.id,
        "title": q.title,
        "status": q.status.as_str(),
        "goal": q.goal,
        "concerns": q.concerns,
        "developments": q.developments,
    })
}
