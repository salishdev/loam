//! The pressure tools the director calls.

use serde::Deserialize;
use serde_json::{Value, json};

use super::{Pressure, PressureError, adjust, resolve, start};
use crate::llm::{ToolDefinition, ToolOutput, ToolUse};
use crate::time;
use crate::world::{EntityId, StoreError, World};

/// `start_pressure`, the one pressure tool the bible phase gets.
pub fn start_definition() -> ToolDefinition {
    ToolDefinition::new(
        "start_pressure",
        "Start a pressure: something that will happen unless someone acts (a sluice giving \
         way, a rival reaching the relic first, a fever turning). The engine tracks how close \
         it is and reminds you; it never makes it happen itself. Use it to give the world \
         momentum, not to punish; the player should be able to sense it coming and push it \
         back.",
        json!({
            "type": "object",
            "properties": {
                "what": {"type": "string", "description": "What is coming, a short phrase."},
                "consequence": {"type": "string", "description": "What happens when it comes."},
                "due_in_minutes": {"type": "integer", "minimum": 1, "description": "World minutes from now until it is due."},
                "concerns": {"type": "array", "items": {"type": "integer"}, "description": "Ids of the entities it threatens. Name a quest to give it a soft deadline: when overdue you are nudged to escalate the quest; it never fails by itself."}
            },
            "required": ["what", "consequence", "due_in_minutes"]
        }),
    )
}

/// Schemas for every pressure tool.
pub fn tool_definitions() -> Vec<ToolDefinition> {
    let pressure = json!({"type": "integer", "description": "The pressure's id."});
    vec![
        start_definition(),
        ToolDefinition::new(
            "adjust_pressure",
            "Move an active pressure's due time when the world pushes it: positive minutes \
             delay it (the player shored up the sluice), negative hasten it (the rain got \
             worse).",
            json!({
                "type": "object",
                "properties": {
                    "id": pressure,
                    "minutes": {"type": "integer", "description": "Positive delays, negative hastens."},
                    "reason": {"type": "string"}
                },
                "required": ["id", "minutes", "reason"]
            }),
        ),
        ToolDefinition::new(
            "resolve_pressure",
            "End an active pressure: it happened (make the consequence real with other tools \
             first) or it was defused.",
            json!({
                "type": "object",
                "properties": {
                    "id": pressure,
                    "how": {"type": "string", "description": "What became of it, one sentence."}
                },
                "required": ["id", "how"]
            }),
        ),
    ]
}

#[derive(Deserialize)]
struct StartInput {
    what: String,
    consequence: String,
    due_in_minutes: u64,
    #[serde(default)]
    concerns: Vec<EntityId>,
}

#[derive(Deserialize)]
struct AdjustInput {
    id: EntityId,
    minutes: i64,
    reason: String,
}

#[derive(Deserialize)]
struct ResolveInput {
    id: EntityId,
    how: String,
}

/// Run a pressure tool call; `None` if `call` isn't one. `Err` only for
/// storage failures.
pub fn handle_tool(world: &mut World, call: &ToolUse) -> Option<Result<ToolOutput, StoreError>> {
    fn parse<T: for<'de> Deserialize<'de>>(call: &ToolUse) -> Result<T, PressureError> {
        serde_json::from_value(call.input.clone())
            .map_err(|e| PressureError::Refused(format!("bad input for {}: {e}", call.name)))
    }
    let result = match call.name.as_str() {
        "start_pressure" => parse::<StartInput>(call).and_then(|i| {
            start(
                world,
                &i.what,
                &i.consequence,
                i.due_in_minutes,
                &i.concerns,
            )
            .and_then(|id| Ok(view(world, &super::existing(world, id)?)?))
        }),
        "adjust_pressure" => parse::<AdjustInput>(call)
            .and_then(|i| adjust(world, i.id, i.minutes, &i.reason))
            .and_then(|p| Ok(view(world, &p)?)),
        "resolve_pressure" => parse::<ResolveInput>(call)
            .and_then(|i| resolve(world, i.id, &i.how))
            .and_then(|p| Ok(view(world, &p)?)),
        _ => return None,
    };
    Some(match result {
        Ok(v) => Ok(ToolOutput::Ok(v.to_string())),
        Err(PressureError::Refused(r)) => Ok(ToolOutput::Err(r)),
        Err(PressureError::Store(e)) => Err(e),
    })
}

/// The ids a pressure tool call would touch: the pressure and every entity
/// it concerns or would concern. `None` if `call` isn't a pressure tool or
/// can't be read (the handler then refuses it).
pub(crate) fn call_concerns(
    world: &World,
    call: &ToolUse,
) -> Result<Option<Vec<EntityId>>, StoreError> {
    let id = match call.name.as_str() {
        "start_pressure" => {
            return Ok(serde_json::from_value::<StartInput>(call.input.clone())
                .ok()
                .map(|i| i.concerns));
        }
        "adjust_pressure" | "resolve_pressure" => call.input.get("id").and_then(Value::as_i64),
        _ => None,
    };
    Ok(match id {
        Some(id) => super::pressure(world, id)?.map(|p| p.concerns),
        None => None,
    })
}

fn view(world: &World, p: &Pressure) -> Result<Value, StoreError> {
    let now = time::now(world)?;
    Ok(json!({
        "pressure": p.id,
        "what": p.what,
        "consequence": p.consequence,
        "status": p.status.as_str(),
        "due": p.due_text(now),
        "urgency": (p.status == super::Status::Active).then(|| p.urgency(now).as_str()),
        "concerns": p.concerns,
        "resolution": p.resolution,
    }))
}
