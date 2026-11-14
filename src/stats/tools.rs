//! The stat tools the director calls: `write_stats` (with the bible), and
//! during play `check`, `adjust_stat`, `add_stat` and `mint_advancement`.

use serde::Deserialize;
use serde_json::{Value, json};

use super::{AdvancementSpec, Changes, StatSpec};
use crate::llm::{ToolDefinition, ToolOutput, ToolUse};
use crate::rules::RuleId;
use crate::world::{StoreError, World};

fn consequence_props() -> Value {
    json!({
        "outcome": {"type": "string", "description": "What happens, for the narrator. Make it a setback, never the end of the game."},
        "effects": {"type": "array", "description": "apply_patch ops; \"player\" and \"room\" name the player and their room.",
                    "items": {"type": "object", "properties": {"op": {"type": "string"}}, "required": ["op"]}},
        "adjust": {"type": "array", "items": {"type": "object", "properties": {
            "stat": {"type": "string"}, "by": {"type": "integer"}}, "required": ["stat", "by"]}},
        "check": {"type": "object", "description": "Optional check(stat, difficulty): outcome/effects/adjust above apply \
                   on success; partial and failure ({outcome, effects?, adjust?}) otherwise."}
    })
}

fn stat_schema() -> Value {
    let mut threshold = json!({
        "type": "object",
        "properties": {
            "name": {"type": "string"},
            "at": {"type": "integer"},
            "when": {"type": "string", "enum": ["falls_to", "rises_to"]}
        },
        "required": ["at", "when", "outcome"]
    });
    for (k, v) in consequence_props().as_object().unwrap() {
        threshold["properties"][k] = v.clone();
    }
    json!({
        "type": "object",
        "properties": {
            "name": {"type": "string", "description": "lowercase, e.g. nerve"},
            "about": {"type": "string", "description": "What it measures, in a phrase."},
            "min": {"type": "integer"}, "max": {"type": "integer"}, "start": {"type": "integer"},
            "thresholds": {"type": "array", "items": threshold,
                           "description": "Fire when the value crosses at. Use them for setbacks and turning points."},
            "capacity": {"type": "object", "description": "Optional: this stat drives the carry limit = base + per_point * value.",
                         "properties": {"capacity": {"type": "string", "enum": ["carry_limit"]},
                                        "base": {"type": "number"}, "per_point": {"type": "number"}},
                         "required": ["capacity", "base", "per_point"]},
            "advancement": {"type": "array", "minItems": 1, "items": advancement_schema(),
                            "description": "How the player can grow it: the advancement charter."}
        },
        "required": ["name", "about", "min", "max", "start", "advancement"]
    })
}

fn advancement_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": {"type": "string"},
            "when": {"type": "string", "description": "What earns growth, e.g. \"facing a horror without fleeing\"."},
            "step": {"type": "integer", "minimum": 1, "description": "Most it raises the stat at once."}
        },
        "required": ["when", "step"]
    })
}

/// `write_stats`, offered while building the bible.
pub fn sheet_tool_definition() -> ToolDefinition {
    ToolDefinition::new(
        "write_stats",
        "Store the player's stat sheet, after write_bible. Call once. 2-5 stats fitting the \
         seed, with small ranges (e.g. 0-5): a check rolls d20 + stat against a difficulty. \
         Each stat's rules are fixed forever: bounds, thresholds, an optional capacity and at \
         least one advancement rule.",
        json!({
            "type": "object",
            "properties": {"stats": {"type": "array", "minItems": 1, "items": stat_schema()}},
            "required": ["stats"]
        }),
    )
}

/// The tools offered during play.
pub fn tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition::new(
            "check",
            "Roll check(stat, difficulty) for an uncertain action: d20 + stat against the \
             difficulty (10 routine, 15 hard, 20 heroic). Success: it works. Partial (within 4 \
             below): it works at a cost or only in part. Failure: it doesn't, and things get \
             worse. Narrate the result you get.",
            json!({
                "type": "object",
                "properties": {
                    "stat": {"type": "string"},
                    "difficulty": {"type": "integer"},
                    "reason": {"type": "string", "description": "What is being attempted."}
                },
                "required": ["stat", "difficulty", "reason"]
            }),
        ),
        ToolDefinition::new(
            "adjust_stat",
            "Change a stat by a logged reason. Lowering needs only the reason; raising must \
             cite one of the stat's advancement rules (rule) and stays within its step. Values \
             stay within bounds. Crossing a threshold fires its rule; narrate its outcome.",
            json!({
                "type": "object",
                "properties": {
                    "stat": {"type": "string"},
                    "by": {"type": "integer"},
                    "reason": {"type": "string"},
                    "rule": {"type": "integer", "description": "Advancement rule id, when raising."}
                },
                "required": ["stat", "by", "reason"]
            }),
        ),
        ToolDefinition::new(
            "add_stat",
            "Add a new stat to the sheet when the story calls for one, with its fixed rules. \
             Stats are never renamed or removed.",
            {
                let mut s = stat_schema();
                s["properties"]["reason"] = json!({"type": "string"});
                s["required"].as_array_mut().unwrap().push(json!("reason"));
                s
            },
        ),
        ToolDefinition::new(
            "mint_advancement",
            "Add a new way for a stat to grow (an advancement rule), e.g. one a quest opens \
             up. It is fixed once minted.",
            {
                let mut s = advancement_schema();
                s["properties"]["stat"] = json!({"type": "string"});
                s["properties"]["reason"] = json!({"type": "string"});
                s["required"] = json!(["stat", "when", "step", "reason"]);
                s
            },
        ),
    ]
}

/// Handle a stat tool call, or `None` if `call` isn't one. `seed` seeds the
/// dice when a sheet is first written.
pub fn handle_tool(
    world: &mut World,
    seed: u64,
    call: &ToolUse,
) -> Option<Result<ToolOutput, StoreError>> {
    macro_rules! input {
        ($t:ty) => {
            match serde_json::from_value::<$t>(call.input.clone()) {
                Ok(v) => v,
                Err(e) => {
                    return Some(Ok(ToolOutput::Err(format!(
                        "bad input for {}: {e}",
                        call.name
                    ))));
                }
            }
        };
    }
    Some(match call.name.as_str() {
        "write_stats" => write_stats(world, seed, input!(SheetInput)),
        "check" => check(world, input!(CheckInput)),
        "adjust_stat" => adjust(world, input!(AdjustInput)),
        "add_stat" => add(world, seed, input!(AddInput)),
        "mint_advancement" => advancement(world, input!(AdvancementInput)),
        _ => return None,
    })
}

fn ok(v: Value) -> Result<ToolOutput, StoreError> {
    Ok(ToolOutput::Ok(v.to_string()))
}

fn refuse(reason: impl Into<String>) -> Result<ToolOutput, StoreError> {
    Ok(ToolOutput::Err(reason.into()))
}

#[derive(Deserialize)]
struct SheetInput {
    stats: Vec<StatSpec>,
}

fn write_stats(world: &mut World, seed: u64, input: SheetInput) -> Result<ToolOutput, StoreError> {
    match super::create_sheet(world, &input.stats, seed)? {
        Ok(rules) => ok(json!({
            "stats": input.stats.iter().map(|s| &s.name).collect::<Vec<_>>(),
            "rules": rules
        })),
        Err(r) => refuse(r),
    }
}

#[derive(Deserialize)]
struct CheckInput {
    stat: String,
    difficulty: i64,
    reason: String,
}

fn check(world: &World, input: CheckInput) -> Result<ToolOutput, StoreError> {
    match super::check(
        world,
        &input.stat,
        input.difficulty,
        json!({"reason": input.reason}),
    )? {
        Ok(roll) => ok(json!({
            "stat": roll.stat, "value": roll.value, "difficulty": roll.difficulty,
            "die": roll.die, "total": roll.total, "result": roll.result,
        })),
        Err(r) => refuse(r),
    }
}

#[derive(Deserialize)]
struct AdjustInput {
    stat: String,
    by: i64,
    reason: String,
    rule: Option<RuleId>,
}

fn adjust(world: &mut World, input: AdjustInput) -> Result<ToolOutput, StoreError> {
    match super::adjust_stat(world, &input.stat, input.by, &input.reason, input.rule)? {
        Ok(changes) => ok(changes_json(&changes)),
        Err(r) => refuse(r),
    }
}

/// Stat changes for a tool result, with a reminder when thresholds fired.
pub fn changes_json(changes: &Changes) -> Value {
    let mut v = json!({"changed": changes.changed});
    if !changes.fired.is_empty() {
        v["thresholds"] = json!(changes.fired);
        v["note"] = json!(
            "A threshold fired and the engine applied its rule: narrate its outcome as a \
             setback or turning point; the game goes on."
        );
    }
    v
}

#[derive(Deserialize)]
struct AddInput {
    #[serde(flatten)]
    spec: StatSpec,
    reason: String,
}

fn add(world: &mut World, seed: u64, input: AddInput) -> Result<ToolOutput, StoreError> {
    match super::add_stat(world, &input.spec, seed, &input.reason)? {
        Ok(rules) => ok(json!({"stat": input.spec.name, "rules": rules})),
        Err(r) => refuse(r),
    }
}

#[derive(Deserialize)]
struct AdvancementInput {
    stat: String,
    reason: String,
    #[serde(flatten)]
    spec: AdvancementSpec,
}

fn advancement(world: &mut World, input: AdvancementInput) -> Result<ToolOutput, StoreError> {
    match super::mint_advancement(world, &input.stat, &input.spec, &input.reason)? {
        Ok(rule) => ok(json!({"rule": rule})),
        Err(r) => refuse(r),
    }
}
