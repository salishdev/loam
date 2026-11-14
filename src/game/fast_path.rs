//! The fast path: Haiku matches the input to a stored rule, the engine
//! applies it, and a Sonnet narrator writes the prose, with no director
//! call. Anything unclear escalates to the director with a note saying why.

use std::fmt::Write;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::llm::{LlmClient, Message, Model, Request, ToolDefinition};
use crate::rules::{self, Bindings, Refusal, Rule, RuleId, Scope};
use crate::world::{EventKind, StoreError, World};

use super::GameError;

const INTERPRETER_SYSTEM: &str = "You match a text adventure player's input against the \
world's stored rules. A rule matches when the input asks for the action its trigger \
describes; bind each of its roles to the id of the entity the input means (only entities \
in the scene). List every rule that could match, best first, or none. Set clean to true only \
when the input is nothing more than that one action. Reply with nothing but the interpret \
tool call.";

const NARRATOR_SYSTEM: &str = "You narrate a text adventure. The engine has already resolved \
the player's action by a known rule; describe exactly that outcome, nothing more: don't \
invent other changes to the world. Second person, present tense, in the world's tone, one \
or two short paragraphs. Output only the prose.";

/// Most tokens the interpreter may spend (thinking included).
const INTERPRETER_MAX_TOKENS: u32 = 4_000;
/// Most tokens the narrator may spend.
const NARRATOR_MAX_TOKENS: u32 = 4_000;

/// What the interpreter's matches come to.
#[derive(Debug)]
pub(crate) enum Verdict {
    /// Exactly one rule applies cleanly.
    Apply { rule: Rule, bindings: Bindings },
    /// The director decides; the note (possibly empty) says why.
    Escalate(String),
}

fn interpret_def() -> ToolDefinition {
    ToolDefinition::new(
        "interpret",
        "Report which stored rules the player's input matches.",
        json!({
            "type": "object",
            "properties": {
                "matches": {"type": "array", "items": {"type": "object", "properties": {
                    "rule": {"type": "integer"},
                    "bindings": {"type": "object", "additionalProperties": {"type": "integer"},
                                 "description": "role -> entity id"}
                }, "required": ["rule", "bindings"]}},
                "clean": {"type": "boolean"},
                "note": {"type": "string"}
            },
            "required": ["matches", "clean"]
        }),
    )
}

#[derive(Debug, Deserialize)]
struct Interpretation {
    #[serde(default)]
    matches: Vec<Match>,
    #[serde(default)]
    clean: bool,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Match {
    rule: RuleId,
    #[serde(default)]
    bindings: Bindings,
}

/// Ask the interpreter which rule `input` matches and decide whether the
/// engine can apply it. Makes no call when there are no active rules.
/// Returns the verdict and the number of model calls made.
pub(crate) fn interpret(
    world: &World,
    client: &dyn LlmClient,
    scope: Scope,
    slice: &str,
    input: &str,
) -> Result<(Verdict, usize), GameError> {
    let active = rules::active(world)?;
    if active.is_empty() {
        return Ok((Verdict::Escalate(String::new()), 0));
    }
    let mut prompt = format!("{slice}\nRules:\n");
    for rule in &active {
        writeln!(prompt, "{}", rule.summary()).unwrap();
    }
    write!(prompt, "\nPlayer input: {input}").unwrap();
    let request = Request::new(Model::Haiku, vec![Message::user(prompt)])
        .system(INTERPRETER_SYSTEM)
        .max_tokens(INTERPRETER_MAX_TOKENS)
        .tools(vec![interpret_def()]);
    // The interpreter is an optimisation: if it fails, the director decides.
    let response = match client.complete(&request) {
        Ok(r) => r,
        Err(e) => {
            world.log_event(
                EventKind::Judgment,
                &json!({"by": "interpreter", "input": input, "error": e.to_string(), "verdict": "escalate"}),
            )?;
            return Ok((Verdict::Escalate(String::new()), 1));
        }
    };

    let raw = response
        .tool_uses()
        .find(|t| t.name == "interpret")
        .map(|t| t.input.clone());
    let parsed = raw.clone().map(serde_json::from_value::<Interpretation>);
    let verdict = match parsed {
        Some(Ok(i)) => decide(world, scope, &active, &i)?,
        Some(Err(_)) | None => Verdict::Escalate(String::new()),
    };
    let mut log = json!({
        "by": "interpreter",
        "input": input,
        "interpretation": raw.unwrap_or(Value::Null),
    });
    match &verdict {
        Verdict::Apply { rule, bindings } => {
            log["verdict"] = json!("fast_path");
            log["rule"] = rules::tag(rule);
            log["bindings"] = json!(bindings);
        }
        Verdict::Escalate(note) => {
            log["verdict"] = json!("escalate");
            if !note.is_empty() {
                log["note"] = json!(note);
            }
        }
    }
    world.log_event(EventKind::Judgment, &log)?;
    Ok((verdict, 1))
}

/// Evaluate the interpreter's matches against the world.
fn decide(
    world: &World,
    scope: Scope,
    active: &[Rule],
    interp: &Interpretation,
) -> Result<Verdict, StoreError> {
    let mut applicable: Vec<(&Rule, Bindings)> = Vec::new();
    let mut misfits: Vec<String> = Vec::new();
    for m in &interp.matches {
        let Some(rule) = active.iter().find(|r| r.id == m.rule) else {
            continue; // not a rule, or retired: the interpreter misread
        };
        if applicable.iter().any(|(r, _)| r.id == rule.id) {
            continue;
        }
        let bindings = match rules::bind(world, &rule.def, scope, &m.bindings)? {
            Ok(b) => b,
            Err(reason) => {
                misfits.push(format!("{}: {reason}", label(rule)));
                continue;
            }
        };
        let unmet = rules::unmet(world, &rule.def, scope, &bindings)?;
        if unmet.is_empty() {
            applicable.push((rule, bindings));
        } else {
            misfits.push(format!("{}: {}", label(rule), unmet.join("; ")));
        }
    }

    Ok(match applicable.len() {
        1 if interp.clean => {
            let (rule, bindings) = applicable.pop().unwrap();
            Verdict::Apply {
                rule: rule.clone(),
                bindings,
            }
        }
        1 => {
            let (rule, _) = &applicable[0];
            let why = interp.note.as_deref().unwrap_or("the input asks for more");
            Verdict::Escalate(format!(
                "{} covers part of this input, but not all of it ({why}). Resolve the whole \
                 input; the rule stays as it is unless it's wrong.",
                label(rule)
            ))
        }
        0 if misfits.is_empty() => Verdict::Escalate(String::new()),
        0 => Verdict::Escalate(format!(
            "Rules matched this input but don't apply here: {}. Rule on this case; if a rule \
             is too narrow or too broad, amend it.",
            misfits.join(" | ")
        )),
        _ => Verdict::Escalate(format!(
            "Conflict: {} all apply to this input. Decide which holds, amend or retire the \
             others, and where you can turn the contradiction into story.",
            applicable
                .iter()
                .map(|(r, _)| label(r))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    })
}

fn label(rule: &Rule) -> String {
    format!("rule #{} {:?}", rule.id, rule.def.name)
}

/// A rule application the engine refused at the last moment (e.g. a
/// destroy of a container that isn't empty): the director takes over.
pub(crate) fn refusal_note(rule: &Rule, refusal: &Refusal) -> String {
    format!(
        "{} matched, but applying it failed: {refusal}. Resolve the input yourself and fix the \
         rule if it is wrong.",
        label(rule)
    )
}

/// Narrate an applied rule, streaming the prose.
pub(crate) fn narrate(
    world: &World,
    client: &dyn LlmClient,
    slice: &str,
    input: &str,
    rule: &Rule,
    application: &rules::Application,
    on_text: &mut dyn FnMut(&str),
) -> Result<crate::llm::Response, GameError> {
    let tone = match super::bible_id(world)?
        .map(|id| world.entity(id))
        .transpose()?
        .flatten()
    {
        Some(bible) => bible
            .props
            .get("tone")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        None => String::new(),
    };
    let mut prompt =
        format!("Tone: {tone}\nScene before the action:\n{slice}\nPlayer input: {input}\n");
    writeln!(prompt, "Resolved by rule: {}", rule.def.name).unwrap();
    if !rule.def.outcome.is_empty() {
        writeln!(prompt, "Outcome: {}", rule.def.outcome).unwrap();
    }
    writeln!(prompt, "Engine changes: {}", json!(application.patch.ops)).unwrap();
    writeln!(prompt, "Involved now:").unwrap();
    for (role, id) in &application.bindings {
        if let Some(e) = world.entity(*id)? {
            writeln!(
                prompt,
                "- {role}: #{} {} [{}] {}",
                e.id,
                e.name,
                e.kind,
                json!(e.props)
            )
            .unwrap();
        }
    }
    for id in &application.applied.created {
        if let Some(e) = world.entity(*id)? {
            writeln!(
                prompt,
                "- new: #{} {} [{}] {}",
                e.id,
                e.name,
                e.kind,
                json!(e.props)
            )
            .unwrap();
        }
    }
    let request = Request::new(Model::Sonnet, vec![Message::user(prompt)])
        .system(NARRATOR_SYSTEM)
        .max_tokens(NARRATOR_MAX_TOKENS);
    Ok(client.send(&request, on_text)?)
}
