//! The rule tools the director calls: `list_rules`, `mint_rule`,
//! `amend_rule`, `retire_rule`.

use serde::Deserialize;
use serde_json::{Value, json};

use super::{Amendment, Bindings, RuleClass, RuleDef, RuleId, Scope};
use crate::glossary;
use crate::llm::{ToolDefinition, ToolOutput, ToolUse};
use crate::world::{StoreError, World};

fn condition_schema() -> Value {
    json!({
        "type": "array",
        "description": "All must hold. check: kind {role, is} | prop {role, prop, is?} (present, or equal to is) \
                        | not_prop {role, prop, is?} (absent, or not equal to is) | carried {role} \
                        | entity {role, id} (pins the rule to one entity; destroying that entity retires the rule) \
                        | related {role, rel, other} (a rel relation runs from role's entity to other role's) \
                        | not_related {role, rel, other} (no such relation). Effects change relations with \
                        relate/unrelate ops.",
        "items": {"type": "object", "properties": {
            "check": {"type": "string", "enum": ["kind", "prop", "not_prop", "carried", "entity", "related", "not_related"]},
            "role": {"type": "string"}, "prop": {"type": "string"}, "is": {}, "id": {"type": "integer"},
            "rel": {"type": "string"}, "other": {"type": "string", "description": "A role name."}
        }, "required": ["check", "role"]}
    })
}

fn effects_schema() -> Value {
    json!({
        "type": "array",
        "description": "apply_patch ops, applied in order. Refer to a bound entity by its role name as a \
                        string (e.g. \"target\", \"room\", \"player\"); an integer id is a fixed entity and pins the rule to it: \
                        destroying that entity retires the rule.",
        "items": {"type": "object", "properties": {"op": {"type": "string"}}, "required": ["op"]}
    })
}

fn adjust_schema() -> Value {
    json!({
        "type": "array",
        "description": "Stat changes after the effects; a world rule may only lower stats (by < 0).",
        "items": {"type": "object", "properties": {"stat": {"type": "string"}, "by": {"type": "integer"}},
                  "required": ["stat", "by"]}
    })
}

fn check_schema() -> Value {
    let branch = json!({"type": "object", "properties": {
        "outcome": {"type": "string"}, "effects": effects_schema(), "adjust": adjust_schema()
    }});
    json!({
        "type": "object",
        "description": "Optional check(stat, difficulty) rolled each time the rule applies. effects, adjust \
                        and outcome above are the success; partial and failure say what happens otherwise \
                        (nothing when absent).",
        "properties": {"stat": {"type": "string"}, "difficulty": {"type": "integer"},
                       "partial": branch.clone(), "failure": branch},
        "required": ["stat", "difficulty"]
    })
}

fn minutes_schema() -> Value {
    json!({
        "type": "integer", "minimum": 0, "maximum": crate::time::MAX_ADVANCE,
        "description": "World minutes the action takes each time the engine applies the rule. \
                        Omitted with apply_to: this turn's duration."
    })
}

fn roles_schema() -> Value {
    json!({
        "type": "object",
        "description": "Roles the input binds, name -> what fills it (e.g. {\"target\": \"the door kicked\"}). \
                        player and room are built in.",
        "additionalProperties": {"type": "string"}
    })
}

pub fn tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition::new(
            "list_rules",
            "List the world's rules (id, name, trigger, roles, conditions, version). Active world \
             rules only unless include_retired or include_stat (the fixed stat rules: bounds, \
             thresholds, capacity, advancement); text filters names and triggers; id shows one \
             rule in full.",
            json!({
                "type": "object",
                "properties": {
                    "text": {"type": "string"},
                    "id": {"type": "integer"},
                    "include_retired": {"type": "boolean"},
                    "include_stat": {"type": "boolean"}
                }
            }),
        ),
        ToolDefinition::new(
            "mint_rule",
            "Record a ruling as a rule the engine applies by itself next time the player does the \
             same thing to a matching entity. Choose the generality with conditions: kind and props \
             make it general, an entity condition pins it to one thing, related/not_related make it \
             hold only while a relation between two roles does (or doesn't). Give apply_to (role -> \
             entity id) to apply it now as this turn's ruling: then nothing is stored unless the \
             conditions hold and the effects apply.",
            json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Short label, e.g. \"Kicking a rotten door\"."},
                    "trigger": {"type": "string", "description": "What the player does, in words, e.g. \"kick, shoulder or force <target>\"."},
                    "roles": roles_schema(),
                    "conditions": condition_schema(),
                    "effects": effects_schema(),
                    "outcome": {"type": "string", "description": "What happens, for the narrator."},
                    "adjust": adjust_schema(),
                    "check": check_schema(),
                    "minutes": minutes_schema(),
                    "apply_to": {"type": "object", "additionalProperties": {"type": "integer"}}
                },
                "required": ["name", "trigger", "roles", "conditions", "effects", "outcome"]
            }),
        ),
        ToolDefinition::new(
            "amend_rule",
            "Replace parts of a world rule (a new version; the old one stays in the log). Use it \
             when a rule proves too broad, too narrow or wrong. Stat rules never change.",
            json!({
                "type": "object",
                "properties": {
                    "id": {"type": "integer"},
                    "reason": {"type": "string"},
                    "name": {"type": "string"},
                    "trigger": {"type": "string"},
                    "roles": roles_schema(),
                    "conditions": condition_schema(),
                    "effects": effects_schema(),
                    "outcome": {"type": "string"},
                    "adjust": adjust_schema(),
                    "check": check_schema(),
                    "minutes": minutes_schema()
                },
                "required": ["id", "reason"]
            }),
        ),
        ToolDefinition::new(
            "retire_rule",
            "Retire a world rule so it no longer applies. Stat rules never change.",
            json!({
                "type": "object",
                "properties": {"id": {"type": "integer"}, "reason": {"type": "string"}},
                "required": ["id", "reason"]
            }),
        ),
    ]
}

/// Handle a rule tool call, or `None` if `call` isn't one. `scope` is
/// needed only by `mint_rule` with `apply_to`.
pub fn handle_tool(
    world: &mut World,
    scope: Option<Scope>,
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
        "list_rules" => list(world, input!(ListInput)),
        "mint_rule" => mint(world, scope, input!(MintInput)),
        "amend_rule" => amend(world, input!(AmendInput)),
        "retire_rule" => retire(world, input!(RetireInput)),
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
struct ListInput {
    text: Option<String>,
    id: Option<RuleId>,
    #[serde(default)]
    include_retired: bool,
    #[serde(default)]
    include_stat: bool,
}

fn list(world: &World, input: ListInput) -> Result<ToolOutput, StoreError> {
    if let Some(id) = input.id {
        return match super::get(world, id)? {
            Some(rule) => {
                let mut v = serde_json::to_value(&rule).expect("rule serializes");
                v["id"] = json!(rule.id);
                ok(v)
            }
            None => refuse(format!("there is no rule #{id}")),
        };
    }
    let text = input.text.map(|t| t.to_lowercase());
    let rules: Vec<Value> = super::all(world)?
        .into_iter()
        .filter(|r| input.include_retired || r.is_active())
        .filter(|r| input.include_stat || r.class == RuleClass::World)
        .filter(|r| {
            text.as_ref().is_none_or(|t| {
                r.def.name.to_lowercase().contains(t) || r.def.trigger.to_lowercase().contains(t)
            })
        })
        .map(|r| {
            let mut v = r.summary();
            if !r.is_active() {
                v["status"] = json!(r.status);
            }
            v
        })
        .collect();
    ok(json!({"rules": rules}))
}

#[derive(Deserialize)]
struct MintInput {
    #[serde(flatten)]
    def: RuleDef,
    apply_to: Option<Bindings>,
}

fn mint(
    world: &mut World,
    scope: Option<Scope>,
    input: MintInput,
) -> Result<ToolOutput, StoreError> {
    if let Err(r) = super::validate(&input.def) {
        return refuse(r);
    }
    if let Err(r) = super::validate_world(world, &input.def)? {
        return refuse(r);
    }
    // Apply first, so a rule is stored only if it really is this turn's ruling.
    let mut result = json!({});
    let mut new_terms = Vec::new();
    if let Some(given) = &input.apply_to {
        let Some(scope) = scope else {
            return refuse("the player is nowhere yet, so apply_to can't be used");
        };
        match super::apply(world, &input.def, json!({"minting": true}), scope, given)? {
            Ok(app) => {
                result["applied"] =
                    json!({"bindings": app.bindings, "created": app.applied.created});
                if let Some(roll) = &app.roll {
                    result["applied"]["check"] = json!({
                        "stat": roll.stat, "die": roll.die, "total": roll.total,
                        "difficulty": roll.difficulty, "result": roll.result,
                    });
                    result["applied"]["outcome"] = json!(app.outcome);
                }
                if !app.stats.is_empty() {
                    result["applied"]["stats"] = crate::stats::tools::changes_json(&app.stats);
                }
                new_terms = app.new_terms;
            }
            Err(refusal) => return refuse(format!("rule not minted: {refusal}")),
        }
    }
    match super::mint(world, input.def, "")? {
        Ok((rule, more)) => {
            for t in more {
                if !new_terms.contains(&t) {
                    new_terms.push(t);
                }
            }
            result["rule"] = json!(rule.id);
            result["version"] = json!(rule.version);
            ok(glossary::tools::with_new_terms(result, &new_terms))
        }
        Err(r) => refuse(r),
    }
}

#[derive(Deserialize)]
struct AmendInput {
    id: RuleId,
    reason: String,
    #[serde(flatten)]
    changes: Amendment,
}

fn amend(world: &mut World, input: AmendInput) -> Result<ToolOutput, StoreError> {
    match super::amend(world, input.id, input.changes, &input.reason)? {
        Ok((rule, new_terms)) => ok(glossary::tools::with_new_terms(
            json!({"rule": rule.id, "version": rule.version}),
            &new_terms,
        )),
        Err(r) => refuse(r),
    }
}

#[derive(Deserialize)]
struct RetireInput {
    id: RuleId,
    reason: String,
}

fn retire(world: &mut World, input: RetireInput) -> Result<ToolOutput, StoreError> {
    match super::retire(world, input.id, &input.reason)? {
        Ok(rule) => ok(json!({"rule": rule.id, "version": rule.version, "status": rule.status})),
        Err(r) => refuse(r),
    }
}
