//! The glossary tools the director calls: `lookup_terms`, `define_terms`.

use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{Definition, define, terms};
use crate::llm::{ToolDefinition, ToolOutput, ToolUse};
use crate::world::{StoreError, World};

pub fn tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition::new(
            "lookup_terms",
            "Look up glossary terms (prop names) with their meanings and known values. \
             Give names, a text to search names and meanings for, or neither for all.",
            json!({
                "type": "object",
                "properties": {
                    "names": {"type": "array", "items": {"type": "string"}},
                    "text": {"type": "string"}
                }
            }),
        ),
        ToolDefinition::new(
            "define_terms",
            "Give glossary terms (prop names) a one-line meaning, and optionally the \
             meaning of particular values. New names are registered.",
            json!({
                "type": "object",
                "properties": {"terms": {"type": "array", "minItems": 1, "items": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "meaning": {"type": "string"},
                        "values": {"type": "object", "additionalProperties": {"type": "string"},
                                   "description": "value (as text, e.g. \"true\") -> meaning"},
                        "free_text": {"type": "boolean", "description": "values are prose; don't record them"}
                    },
                    "required": ["name", "meaning"]
                }}},
                "required": ["terms"]
            }),
        ),
    ]
}

/// Handle a glossary tool call, or `None` if `call` isn't one.
pub fn handle_tool(world: &mut World, call: &ToolUse) -> Option<Result<ToolOutput, StoreError>> {
    Some(match call.name.as_str() {
        "lookup_terms" => match parse(call) {
            Ok(i) => lookup(world, i),
            Err(e) => Ok(e),
        },
        "define_terms" => match parse::<DefineInput>(call) {
            Ok(i) => define(world, &i.terms).map(|r| match r {
                Ok(()) => ToolOutput::Ok(json!({"defined": i.terms.len()}).to_string()),
                Err(reason) => ToolOutput::Err(reason),
            }),
            Err(e) => Ok(e),
        },
        _ => return None,
    })
}

/// Add the glossary entries a change introduced to a tool result, asking
/// the director to define them.
pub fn with_new_terms(mut result: Value, new_terms: &[String]) -> Value {
    if !new_terms.is_empty() {
        result["new_glossary_terms"] = json!(new_terms);
        result["note"] = json!("describe the new terms with define_terms");
    }
    result
}

fn parse<T: for<'de> Deserialize<'de>>(call: &ToolUse) -> Result<T, ToolOutput> {
    serde_json::from_value(call.input.clone())
        .map_err(|e| ToolOutput::Err(format!("bad input for {}: {e}", call.name)))
}

#[derive(Deserialize)]
struct LookupInput {
    #[serde(default)]
    names: Vec<String>,
    text: Option<String>,
}

#[derive(Deserialize)]
struct DefineInput {
    terms: Vec<Definition>,
}

fn lookup(world: &World, input: LookupInput) -> Result<ToolOutput, StoreError> {
    let all = terms(world)?;
    let mut found = Map::new();
    let mut missing = Vec::new();
    for name in &input.names {
        match all.get(name) {
            Some(t) => {
                found.insert(
                    name.clone(),
                    serde_json::to_value(t).expect("term serializes"),
                );
            }
            None => missing.push(name.clone()),
        }
    }
    if let Some(text) = input.text.as_deref().map(str::to_lowercase) {
        for (name, t) in &all {
            if name.to_lowercase().contains(&text) || t.meaning.to_lowercase().contains(&text) {
                found.insert(
                    name.clone(),
                    serde_json::to_value(t).expect("term serializes"),
                );
            }
        }
    }
    if input.names.is_empty() && input.text.is_none() {
        for (name, t) in &all {
            found.insert(
                name.clone(),
                serde_json::to_value(t).expect("term serializes"),
            );
        }
    }
    let mut out = json!({"terms": Value::Object(found)});
    if !missing.is_empty() {
        out["not_in_glossary"] = json!(missing);
    }
    Ok(ToolOutput::Ok(out.to_string()))
}
