//! Surgery: edit what the director was told for a turn, then play it.
//!
//! [`prepare`] rebuilds the director's turn message (or its system prompt)
//! for a turn, in an in-memory fork just before it, with no model call.
//! The player edits that text in `$EDITOR`; [`apply`] turns the edit into
//! an [`Edit`], or cancels when nothing changed. [`run`] then plays it as a
//! reroll: `n` variants with the edited prompt (marked ✂), then one
//! control with the prompt unedited. Every variant goes straight to the
//! director, so a fast-path turn is forced onto the director path, and the
//! control differs from the edited variants only by the edit. The
//! `rerolls` row's knobs record `surgery`: the target, the edit's diff and
//! which variant is the control.

use serde_json::{Value, json};

use super::{MAX_N, Mark, Plan, Progress, Reroll, turn_input};
use crate::game::{DirectorPrompt, Game};
use crate::llm::{Brains, LlmClient};
use crate::world::World;
use crate::xray;

/// Edited variants when `:surgery` doesn't say; with the control, as many
/// turns as a default reroll.
pub const DEFAULT_EDITED: usize = 2;
/// Most edited variants: with the control, at most [`MAX_N`] turns.
pub const MAX_EDITED: usize = MAX_N - 1;

/// What `:surgery` accepts.
pub const USAGE: &str = "Usage: :surgery [#turn] [system] [n] \
[director=<haiku|sonnet|opus>] [effort=<low|medium|high|xhigh|max>] [dice=fresh]";

/// Which part of the director's prompt is edited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Target {
    /// The turn's user message: slice, pressures, notes and input.
    #[default]
    Message,
    /// The director's system prompt.
    System,
}

impl Target {
    pub fn as_str(self) -> &'static str {
        match self {
            Target::Message => "message",
            Target::System => "system",
        }
    }
}

/// What to operate on and how: `reroll.n` counts the edited variants.
#[derive(Debug, Clone, PartialEq)]
pub struct Spec {
    pub target: Target,
    pub reroll: super::Spec,
}

impl Spec {
    /// Parse `:surgery`'s argument: `[#seq] [system|message] [n]
    /// [director=<model>] [effort=<level>] [dice=fresh|same]`, any order.
    pub fn parse(arg: &str) -> Result<Spec, String> {
        let mut target = Target::Message;
        let mut rest = Vec::new();
        let mut counted = false;
        for word in arg.split_whitespace() {
            match word {
                "system" => target = Target::System,
                "message" => target = Target::Message,
                "show" => return Err(USAGE.into()),
                _ => {
                    if let Ok(n) = word.parse::<usize>() {
                        if !(1..=MAX_EDITED).contains(&n) {
                            return Err(too_many());
                        }
                        counted = true;
                    }
                    rest.push(word);
                }
            }
        }
        let mut reroll = super::Spec::parse(&rest.join(" ")).map_err(|e| {
            if e == super::USAGE {
                USAGE.to_string()
            } else {
                e
            }
        })?;
        if !counted {
            reroll.n = DEFAULT_EDITED;
        }
        Ok(Spec { target, reroll })
    }
}

fn too_many() -> String {
    format!("A surgery runs 1 to {MAX_EDITED} edited variants (plus the control).")
}

/// The text to edit, for a turn.
#[derive(Debug, Clone, PartialEq)]
pub struct Prepared {
    /// The turn's input event.
    pub turn_seq: i64,
    pub input: String,
    pub target: Target,
    /// What the director would be told, unedited.
    pub prompt: DirectorPrompt,
}

impl Prepared {
    /// The text the player edits.
    pub fn text(&self) -> &str {
        match self.target {
            Target::Message => &self.prompt.message,
            Target::System => &self.prompt.system,
        }
    }
}

/// Build the director's prompt for the turn `spec` names (the latest when
/// it names none), from `world` just before that turn. No model call;
/// `world` may be a read-only view. `Err` is what to tell the player.
pub fn prepare(world: &World, spec: &Spec) -> Result<Prepared, String> {
    let failed = |e: &dyn std::fmt::Display| format!("[surgery failed: {e}]");
    let turn_seq = match spec.reroll.turn {
        Some(seq) => seq,
        None => xray::latest_turn(world)
            .map_err(|e| failed(&e))?
            .ok_or("No turn to operate on yet.")?,
    };
    let input = turn_input(world, turn_seq)
        .map_err(|e| failed(&e))?
        .ok_or_else(|| format!("Event #{turn_seq} isn't a turn's input; :xray shows turns."))?;
    let fork = world.fork_in_memory(turn_seq - 1).map_err(|e| failed(&e))?;
    let prompt = Game::new(fork)
        .director_prompt(&input)
        .map_err(|e| failed(&e))?;
    Ok(Prepared {
        turn_seq,
        input,
        target: spec.target,
        prompt,
    })
}

/// A surgery ready to run: the prompt before and after the edit.
#[derive(Debug, Clone, PartialEq)]
pub struct Edit {
    pub prepared: Prepared,
    pub edited: DirectorPrompt,
    /// The edit as a word diff ([`word_diff`]).
    pub diff: String,
}

/// The player's edit of `prepared`'s text, or `None` when it is empty or
/// unchanged (trailing whitespace aside): the surgery is then cancelled
/// and no model is called. The original's trailing whitespace is kept, so
/// a newline an editor adds doesn't count.
pub fn apply(prepared: Prepared, edited: &str) -> Option<Edit> {
    let original = prepared.text();
    let body = edited.trim_end();
    if body.trim().is_empty() || body == original.trim_end() {
        return None;
    }
    let text = format!("{body}{}", &original[original.trim_end().len()..]);
    let diff = word_diff(original, &text);
    let mut prompt = prepared.prompt.clone();
    match prepared.target {
        Target::Message => prompt.message = text,
        Target::System => prompt.system = text,
    }
    Some(Edit {
        prepared,
        edited: prompt,
        diff,
    })
}

/// What the worker runs: the options and the edit.
#[derive(Debug, Clone, PartialEq)]
pub struct Surgery {
    pub spec: Spec,
    pub edit: Edit,
}

impl Surgery {
    /// The `rerolls.knobs` this surgery stores.
    pub fn knobs(&self) -> Value {
        let mut knobs = self.spec.reroll.knobs();
        knobs["surgery"] = json!({
            "target": self.spec.target.as_str(),
            "diff": self.edit.diff,
            "control": self.spec.reroll.n + 1,
        });
        knobs
    }
}

/// Run `surgery` against `world` as a reroll of its turn: variants 1 to n
/// with the edited prompt, then the control. Like [`super::run`] otherwise.
pub fn run<'c>(
    world: &World,
    surgery: &Surgery,
    session: &Brains,
    clients: &dyn Fn(usize) -> &'c (dyn LlmClient + Sync),
    progress: &mut dyn FnMut(Progress),
) -> Result<Reroll, String> {
    let edit = &surgery.edit;
    let mut spec = surgery.spec.reroll.clone();
    spec.turn = Some(edit.prepared.turn_seq);
    let mut plans = vec![Plan::Prompt(edit.edited.clone(), Mark::Edited); spec.n];
    plans.push(Plan::Prompt(edit.prepared.prompt.clone(), Mark::Control));
    spec.n = plans.len();
    super::run_plans(
        world,
        &spec,
        &surgery.knobs(),
        &plans,
        session,
        clients,
        progress,
    )
}

/// Words of context shown around a change.
const CONTEXT_WORDS: usize = 6;
/// Changes this close (in unchanged words) share a hunk.
const MERGE_WORDS: usize = 4;
/// Beyond this many token pairs, the changed middle is shown as one
/// replacement rather than diffed.
const MAX_DIFF_CELLS: usize = 4_000_000;

/// Words with the whitespace after them, so they join back losslessly.
fn tokens(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut in_space = false;
    for (i, c) in text.char_indices() {
        if c.is_whitespace() {
            in_space = true;
        } else if in_space {
            out.push(&text[start..i]);
            start = i;
            in_space = false;
        }
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Op {
    Same,
    Del,
    Ins,
}

/// `old` → `new` as hunks of changed words:
///
/// ```text
/// @@ …the words just before
/// - what was removed
/// + what was added
/// ```
///
/// Whitespace is collapsed for display and ignored when matching words;
/// an empty side is left out.
pub fn word_diff(old: &str, new: &str) -> String {
    let (a, b) = (tokens(old), tokens(new));
    let prefix = a.iter().zip(&b).take_while(|(x, y)| same(x, y)).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(x, y)| same(x, y))
        .count();
    let (am, bm) = (&a[prefix..a.len() - suffix], &b[prefix..b.len() - suffix]);
    let mut ops: Vec<(Op, &str)> = a[..prefix].iter().map(|t| (Op::Same, *t)).collect();
    ops.extend(middle(am, bm));
    ops.extend(a[a.len() - suffix..].iter().map(|t| (Op::Same, *t)));

    // Hunks: spans of changes, merging those separated by few same words.
    let mut hunks: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < ops.len() {
        if ops[i].0 == Op::Same {
            i += 1;
            continue;
        }
        let start = i;
        let mut end = i;
        while end < ops.len() {
            if ops[end].0 != Op::Same {
                end += 1;
                continue;
            }
            let same = ops[end..].iter().take_while(|o| o.0 == Op::Same).count();
            if end + same < ops.len() && same <= MERGE_WORDS {
                end += same;
            } else {
                break;
            }
        }
        hunks.push((start, end));
        i = end;
    }

    let show = |words: Vec<&str>| {
        words
            .concat()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    let mut out = Vec::new();
    for (start, end) in hunks {
        let before: Vec<&str> = ops[start.saturating_sub(CONTEXT_WORDS)..start]
            .iter()
            .filter(|o| o.0 == Op::Same)
            .map(|o| o.1)
            .collect();
        let context = show(before);
        out.push(if context.is_empty() {
            "@@ (start)".to_string()
        } else {
            format!("@@ …{context}")
        });
        let span = &ops[start..end];
        let del = show(
            span.iter()
                .filter(|o| o.0 != Op::Ins)
                .map(|o| o.1)
                .collect(),
        );
        let ins = show(
            span.iter()
                .filter(|o| o.0 != Op::Del)
                .map(|o| o.1)
                .collect(),
        );
        if !del.is_empty() {
            out.push(format!("- {del}"));
        }
        if !ins.is_empty() {
            out.push(format!("+ {ins}"));
        }
    }
    if out.is_empty() && old != new {
        return "(whitespace only)".into();
    }
    out.join("\n")
}

/// Words match whatever whitespace follows them: whitespace is collapsed
/// in the diff anyway.
fn same(a: &str, b: &str) -> bool {
    a.trim_end() == b.trim_end()
}

/// The changed middle as a longest-common-subsequence edit script, or one
/// whole replacement when it is too big to diff.
fn middle<'t>(a: &[&'t str], b: &[&'t str]) -> Vec<(Op, &'t str)> {
    let del = |t: &&'t str| (Op::Del, *t);
    let ins = |t: &&'t str| (Op::Ins, *t);
    if a.is_empty() || b.is_empty() || a.len() * b.len() > MAX_DIFF_CELLS {
        return a.iter().map(del).chain(b.iter().map(ins)).collect();
    }
    let w = b.len() + 1;
    // lcs[i * w + j]: the LCS length of a[i..] and b[j..].
    let mut lcs = vec![0u32; (a.len() + 1) * w];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i * w + j] = if same(a[i], b[j]) {
                lcs[(i + 1) * w + j + 1] + 1
            } else {
                lcs[(i + 1) * w + j].max(lcs[i * w + j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::with_capacity(a.len() + b.len());
    while i < a.len() && j < b.len() {
        if same(a[i], b[j]) {
            out.push((Op::Same, a[i]));
            i += 1;
            j += 1;
        } else if lcs[(i + 1) * w + j] >= lcs[i * w + j + 1] {
            out.push(del(&a[i]));
            i += 1;
        } else {
            out.push(ins(&b[j]));
            j += 1;
        }
    }
    out.extend(a[i..].iter().map(del));
    out.extend(b[j..].iter().map(ins));
    out
}

/// Most diff lines the reroll overview shows.
const PREVIEW_LINES: usize = 8;

/// The edit's diff for the top of the reroll view and report: a heading,
/// then the first few lines, clipped.
pub fn diff_preview(diff: &str) -> Vec<String> {
    let lines: Vec<&str> = diff.lines().collect();
    let mut out = vec!["✂ edit:".to_string()];
    out.extend(
        lines
            .iter()
            .take(PREVIEW_LINES)
            .map(|l| format!("  {}", xray::clip(l, 100))),
    );
    if lines.len() > PREVIEW_LINES {
        out.push(format!(
            "  … {} more lines (:rerolls shows the whole diff)",
            lines.len() - PREVIEW_LINES
        ));
    }
    out
}
