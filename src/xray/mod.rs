//! The X-ray: what the models did on a turn, read back from the save.
//!
//! [`turn`] gathers one turn's recorded calls ([`World::turn_calls`]) and
//! what the engine made of them from the log: refusals, rule changes and
//! rolls. Both front ends show it: the TUI as the X-ray panel, plain mode
//! as a dim [`footer`] after each turn. [`tree`] lays one call out as the
//! context viewer's foldable tree. Nothing here calls a model.

pub mod tree;

use std::fmt::Write as _;

use serde_json::Value;

use crate::world::{Event, EventKind, StoreError, StoredCall, World};

/// How a turn was resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Path {
    /// The director ruled on the input.
    Director,
    /// The interpreter matched a stored rule and the engine applied it.
    FastPath(Option<i64>),
}

impl Path {
    pub fn label(&self) -> String {
        match self {
            Path::Director => "director".into(),
            Path::FastPath(Some(rule)) => format!("fast path rule #{rule}"),
            Path::FastPath(None) => "fast path".into(),
        }
    }
}

/// One recorded call, summed up.
#[derive(Debug, Clone, PartialEq)]
pub struct CallLine {
    /// Its place in the turn, from 1 (`:context #seq.N`).
    pub n: usize,
    pub id: i64,
    pub role: String,
    /// The model's short name (`haiku`, `sonnet`).
    pub model: String,
    pub latency_ms: u64,
    /// The whole prompt: uncached input, cache writes and cache reads.
    pub prompt_tokens: u64,
    /// The part of the prompt read from the cache.
    pub cached_tokens: u64,
    pub output_tokens: u64,
    /// Tool calls in the response.
    pub tool_calls: usize,
    pub cost_usd: Option<f64>,
}

impl CallLine {
    pub fn new(n: usize, stored: &StoredCall) -> CallLine {
        let c = &stored.call;
        let u = &c.usage;
        let tool_calls = c.response["content"].as_array().map_or(0, |blocks| {
            blocks.iter().filter(|b| b["type"] == "tool_use").count()
        });
        CallLine {
            n,
            id: stored.id,
            role: c.role.clone(),
            model: short_model(&c.model),
            latency_ms: c.latency_ms,
            prompt_tokens: u.input_tokens
                + u.cache_creation_input_tokens
                + u.cache_read_input_tokens,
            cached_tokens: u.cache_read_input_tokens,
            output_tokens: u.output_tokens,
            tool_calls,
            cost_usd: c.cost_usd,
        }
    }

    /// `2 director · sonnet · 2.1s · 2 tool calls`
    pub fn head(&self) -> String {
        let mut s = format!(
            "{} {} · {} · {}",
            self.n,
            self.role,
            self.model,
            seconds(self.latency_ms)
        );
        match self.tool_calls {
            0 => {}
            1 => s.push_str(" · 1 tool call"),
            n => write!(s, " · {n} tool calls").unwrap(),
        }
        s
    }

    /// `in 5.0k (81% cached) · out 300 · $0.0081`
    pub fn usage(&self) -> String {
        format!(
            "in {} ({}% cached) · out {} · {}",
            tokens(self.prompt_tokens),
            percent(self.cached_tokens, self.prompt_tokens),
            tokens(self.output_tokens),
            dollars(self.cost_usd)
        )
    }
}

/// One turn as the X-ray sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnXray {
    /// The turn's `input` event.
    pub seq: i64,
    pub input: String,
    pub path: Path,
    /// False when the turn was played before the save recorded calls.
    pub recorded: bool,
    pub calls: Vec<CallLine>,
    /// What the turn's calls cost (unpriced models count as nothing).
    pub cost_usd: f64,
    /// Patches the engine refused and tool calls that errored.
    pub refusals: Vec<String>,
    /// Rules minted, amended, retired or timed.
    pub rules: Vec<String>,
    pub rolls: Vec<String>,
}

impl TurnXray {
    pub fn prompt_tokens(&self) -> u64 {
        self.calls.iter().map(|c| c.prompt_tokens).sum()
    }

    pub fn cached_tokens(&self) -> u64 {
        self.calls.iter().map(|c| c.cached_tokens).sum()
    }

    pub fn output_tokens(&self) -> u64 {
        self.calls.iter().map(|c| c.output_tokens).sum()
    }

    /// Why there are no calls to show, when there are none.
    pub fn no_calls(&self) -> Option<&'static str> {
        if !self.calls.is_empty() {
            None
        } else if !self.recorded {
            Some(PRE_RECORDING)
        } else {
            Some("no model calls recorded")
        }
    }
}

/// What a turn from before recording began says about its calls.
pub const PRE_RECORDING: &str = "no call records: played before this save recorded model calls";

/// The X-ray of the turn whose input is event `seq`, or `None` when that
/// event isn't a turn's input.
pub fn turn(world: &World, seq: i64) -> Result<Option<TurnXray>, StoreError> {
    let Some(input) = world.event(seq)? else {
        return Ok(None);
    };
    if input.kind != EventKind::Input || input.payload["seed"] == Value::Bool(true) {
        return Ok(None);
    }
    let stored = world.turn_calls(seq)?;
    let calls: Vec<CallLine> = stored
        .iter()
        .enumerate()
        .map(|(i, c)| CallLine::new(i + 1, c))
        .collect();
    let events = turn_events(world, seq)?;
    let directed = calls.iter().any(|c| c.role == "director")
        || events.iter().any(|e| e.kind == EventKind::ToolCall);
    let fast_rule = events
        .iter()
        .find(|e| {
            e.kind == EventKind::Judgment
                && e.payload["by"] == "interpreter"
                && e.payload["verdict"] == "fast_path"
        })
        .map(|e| e.payload["rule"]["id"].as_i64());
    let path = match fast_rule {
        Some(rule) if !directed => Path::FastPath(rule),
        _ => Path::Director,
    };
    let mut x = TurnXray {
        seq,
        input: input.payload["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        path,
        recorded: seq >= world.recorded_from()?,
        // Folded from +0.0: an empty f64 `sum` is -0.0, shown as "$-0.000".
        cost_usd: calls
            .iter()
            .filter_map(|c| c.cost_usd)
            .fold(0.0, |a, b| a + b),
        calls,
        refusals: Vec::new(),
        rules: Vec::new(),
        rolls: Vec::new(),
    };
    // A rejected patch is logged inside the tool call that tried it; that
    // call's error says the same thing again, so only one line is kept.
    let mut explained = false;
    for e in &events {
        let p = &e.payload;
        match e.kind {
            EventKind::PatchRejected => {
                x.refusals.push(refusal(e)?);
                explained = true;
            }
            EventKind::ToolCall => {
                if p["is_error"] == Value::Bool(true) && !explained {
                    x.refusals.push(format!(
                        "refused: {}: {}",
                        p["name"].as_str().unwrap_or("?"),
                        clip(p["result"].as_str().unwrap_or_default(), 160)
                    ));
                }
                explained = false;
            }
            EventKind::RuleChange => x.rules.push(format!(
                "rule #{} {}: {}",
                p["id"],
                p["action"].as_str().unwrap_or("changed"),
                p["rule"]["name"].as_str().unwrap_or("?")
            )),
            EventKind::Roll => x.rolls.push(format!(
                "roll: {} d20 {} + {} = {} vs {}: {}",
                p["stat"].as_str().unwrap_or("?"),
                p["die"],
                p["value"],
                p["total"],
                p["difficulty"],
                p["result"].as_str().unwrap_or("?")
            )),
            _ => {}
        }
    }
    Ok(Some(x))
}

/// Call `n` (from 1) of the turn whose input is event `seq`.
pub fn turn_call(world: &World, seq: i64, n: usize) -> Result<Option<StoredCall>, StoreError> {
    Ok(n.checked_sub(1)
        .and_then(|i| world.turn_calls(seq).ok()?.into_iter().nth(i)))
}

/// A call's context tree, its system prompt folded when it matches the
/// previous call of the same role, and the banner for a reroll variant
/// whose system prompt differs from the original turn's.
pub fn call_tree(
    world: &World,
    stored: &StoredCall,
) -> Result<(tree::Tree, Option<String>), StoreError> {
    let previous = world.previous_call(stored.id, &stored.call.role)?;
    let unchanged = stored.system_hash.is_some()
        && previous.is_some_and(|p| p.system_hash == stored.system_hash);
    let banner = match (stored.reroll_id, stored.turn_seq) {
        (Some(_), Some(seq)) => {
            let original = world
                .turn_calls(seq)?
                .into_iter()
                .find(|c| c.call.role == stored.call.role);
            tree::variant_banner(stored, original.as_ref())
        }
        _ => None,
    };
    Ok((tree::Tree::build(stored, unchanged), banner))
}

/// The latest turn's input event, if any turn has been played.
pub fn latest_turn(world: &World) -> Result<Option<i64>, StoreError> {
    Ok(world.turn_seqs()?.last().copied())
}

/// The turn before (`back`) or after `seq`, if there is one.
pub fn step_turn(world: &World, seq: i64, back: bool) -> Result<Option<i64>, StoreError> {
    let seqs = world.turn_seqs()?;
    Ok(if back {
        seqs.iter().rev().find(|&&s| s < seq).copied()
    } else {
        seqs.iter().find(|&&s| s > seq).copied()
    })
}

/// A turn's events after its input: everything up to its narration, then
/// the time, judgments and rule changes the engine logs when the turn
/// ends. A world tick that follows is left out.
fn turn_events(world: &World, seq: i64) -> Result<Vec<Event>, StoreError> {
    let mut out = Vec::new();
    let mut narrated = false;
    for e in world.events_since(seq)? {
        if e.kind == EventKind::Input
            || (narrated
                && !matches!(
                    e.kind,
                    EventKind::Time | EventKind::Judgment | EventKind::RuleChange
                ))
        {
            break;
        }
        narrated |= e.kind == EventKind::Narration;
        out.push(e);
    }
    Ok(out)
}

/// `refused: create "gold" (item) in #1 — not a container`
pub(crate) fn refusal(e: &Event) -> Result<String, StoreError> {
    let p = &e.payload;
    let reason = p["reason"].as_str().unwrap_or("refused").trim();
    let op = p["op"].as_u64().map(|i| i as usize);
    let ops = e.patch_touches()?;
    Ok(match op.and_then(|i| ops.get(i)) {
        Some((op, _)) => format!("refused: {} — {reason}", crate::inspect::render_op(op, &[])),
        None => format!("refused: {reason}"),
    })
}

/// The plain-mode footer: the path and calls, tokens and cost, then one
/// line per refusal, rule change and roll, each in brackets.
pub fn footer(x: &TurnXray) -> String {
    let mut out = format!("[{} · ", x.path.label());
    match x.no_calls() {
        Some(why) => out.push_str(why),
        None => {
            out.push_str(&runs(&x.calls).join(" → "));
            write!(
                out,
                "\n in {} ({}% cached) out {} · {}",
                tokens(x.prompt_tokens()),
                percent(x.cached_tokens(), x.prompt_tokens()),
                tokens(x.output_tokens()),
                dollars(Some(x.cost_usd))
            )
            .unwrap();
        }
    }
    out.push(']');
    for line in x.refusals.iter().chain(&x.rules).chain(&x.rolls) {
        write!(out, "\n[{line}]").unwrap();
    }
    out
}

/// Consecutive calls of one role and model as one run:
/// `haiku interpret 0.4s`, `sonnet director 2 calls 3.1s`.
fn runs(calls: &[CallLine]) -> Vec<String> {
    let mut runs: Vec<(String, String, usize, u64)> = Vec::new();
    for c in calls {
        match runs.last_mut() {
            Some((model, role, n, ms)) if *model == c.model && *role == c.role => {
                *n += 1;
                *ms += c.latency_ms;
            }
            _ => runs.push((c.model.clone(), c.role.clone(), 1, c.latency_ms)),
        }
    }
    runs.into_iter()
        .map(|(model, role, n, ms)| match n {
            1 => format!("{model} {role} {}", seconds(ms)),
            n => format!("{model} {role} {n} calls {}", seconds(ms)),
        })
        .collect()
}

/// The X-ray panel's lines, each with the call it belongs to (for
/// selection), unwrapped.
pub fn panel_lines(x: &TurnXray) -> Vec<(String, Option<usize>)> {
    let mut out: Vec<(String, Option<usize>)> = Vec::new();
    let mut line = |s: String, call: Option<usize>| out.push((s, call));
    line(format!("Turn #{} › {}", x.seq, x.input), None);
    line(
        match x.no_calls() {
            Some(_) => x.path.label(),
            None => format!("{} · {}", x.path.label(), dollars(Some(x.cost_usd))),
        },
        None,
    );
    line(String::new(), None);
    match x.no_calls() {
        Some(why) => line(format!("({why})"), None),
        None => {
            line(
                format!(
                    "Calls: in {} ({}% cached) · out {}",
                    tokens(x.prompt_tokens()),
                    percent(x.cached_tokens(), x.prompt_tokens()),
                    tokens(x.output_tokens())
                ),
                None,
            );
            for (i, c) in x.calls.iter().enumerate() {
                line(c.head(), Some(i));
                line(format!("  {}", c.usage()), Some(i));
            }
        }
    }
    let sections = [
        ("Refusals", &x.refusals),
        ("Rules", &x.rules),
        ("Rolls", &x.rolls),
    ];
    for (title, items) in sections {
        line(String::new(), None);
        if items.is_empty() {
            line(format!("{title}: none"), None);
            continue;
        }
        line(format!("{title}:"), None);
        for item in items {
            line(format!("- {item}"), None);
        }
    }
    out
}

/// `:context [#seq]`: the turn's calls, one per line, each with the
/// `#seq.N` that opens it.
pub fn context_list(x: &TurnXray) -> String {
    let mut out = format!("Turn #{} › {}\n{}", x.seq, x.input, x.path.label());
    match x.no_calls() {
        Some(why) => write!(out, "\n({why})").unwrap(),
        None => {
            for c in &x.calls {
                write!(
                    out,
                    "\n#{}.{} {} · {} · {} · {}",
                    x.seq,
                    c.n,
                    c.role,
                    c.model,
                    seconds(c.latency_ms),
                    c.usage()
                )
                .unwrap();
            }
            write!(
                out,
                "\n:context #{}.<n> shows one call (--raw for its JSON).",
                x.seq
            )
            .unwrap();
        }
    }
    out
}

/// The model's short name: `claude-haiku-5-5` → `haiku`.
pub fn short_model(id: &str) -> String {
    match id.strip_prefix("claude-") {
        Some(rest) => rest.split('-').next().unwrap_or(rest).to_owned(),
        None => id.to_owned(),
    }
}

/// `410`, `6.2k`, `128k`
pub fn tokens(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..99_950 => format!("{:.1}k", n as f64 / 1000.0),
        _ => format!("{}k", (n + 500) / 1000),
    }
}

fn seconds(ms: u64) -> String {
    format!("{:.1}s", ms as f64 / 1000.0)
}

pub(crate) fn percent(part: u64, whole: u64) -> u64 {
    (part * 100).checked_div(whole).unwrap_or(0)
}

fn dollars(cost: Option<f64>) -> String {
    match cost {
        Some(c) => format!("${c:.3}"),
        None => "$?".into(),
    }
}

pub(crate) fn clip(s: &str, max: usize) -> String {
    let s = s.trim();
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_owned(),
    }
}

#[cfg(test)]
pub(crate) mod tests;
