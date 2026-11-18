//! One recorded call as a foldable tree: the context viewer's model.
//!
//! Nodes are kept flat in pre-order, each knowing its parent and depth.
//! Each node has a token count, estimated from the recorded usage by its
//! share of the characters: prompt nodes share the prompt's tokens, and
//! response nodes share the output tokens. A prompt node's cached part is
//! its overlap with the cache-read prefix. The API reads the prompt as
//! tools, then system, then messages, so that is the order the prefix
//! covers, whatever order the tree shows them in.

use std::fmt::Write as _;

use serde_json::{Value, json};

use super::tokens;
use crate::world::StoredCall;

#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub label: String,
    /// Text shown under the label when the node is open.
    pub body: Vec<String>,
    pub parent: Option<usize>,
    pub depth: usize,
    pub tokens: u64,
    /// The part of `tokens` read from the prompt cache.
    pub cached: u64,
    pub folded: bool,
}

impl Node {
    /// Whether the node shows its token count.
    fn counted(&self) -> bool {
        self.tokens > 0
    }
}

/// One line on screen: a node's label (`line: None`) or one line of its
/// body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    pub node: usize,
    pub line: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Tree {
    pub nodes: Vec<Node>,
}

/// Builds nodes and remembers each one's place in the prompt or response,
/// in characters, so tokens can be shared out at the end.
struct Builder {
    nodes: Vec<Node>,
    /// (start, end, in the prompt) per node; parents get their children's
    /// union.
    spans: Vec<Option<(usize, usize, bool)>>,
    stack: Vec<usize>,
}

impl Builder {
    fn open(&mut self, label: impl Into<String>, folded: bool) -> usize {
        let i = self.leaf(label, Vec::new(), None, folded);
        self.stack.push(i);
        i
    }

    fn close(&mut self) {
        self.stack.pop();
    }

    /// A node under the open one. `span` places it in the prompt
    /// (`true`) or the response.
    fn leaf(
        &mut self,
        label: impl Into<String>,
        body: Vec<String>,
        span: Option<(usize, usize, bool)>,
        folded: bool,
    ) -> usize {
        let parent = self.stack.last().copied();
        self.nodes.push(Node {
            label: label.into(),
            body,
            parent,
            depth: self.stack.len(),
            tokens: 0,
            cached: 0,
            folded,
        });
        self.spans.push(span);
        self.nodes.len() - 1
    }
}

/// Character positions in the prompt, in the order the API reads it.
struct Prompt {
    tools: usize,
    system: usize,
    /// Where each message starts.
    messages: Vec<usize>,
    total: usize,
}

impl Tree {
    /// The tree of `stored`. `system_unchanged` folds the system prompt
    /// (it matches the previous call's).
    pub fn build(stored: &StoredCall, system_unchanged: bool) -> Tree {
        let call = &stored.call;
        let tools_text = call
            .tools
            .as_ref()
            .map(|t| serde_json::to_string_pretty(t).unwrap_or_default())
            .unwrap_or_default();
        let system = call.system.clone().unwrap_or_default();
        let empty = Vec::new();
        let messages = call.request["messages"].as_array().unwrap_or(&empty);
        let message_texts: Vec<String> = messages.iter().map(|m| m.to_string()).collect();
        let mut at = tools_text.chars().count() + system.chars().count();
        let mut starts = Vec::new();
        for t in &message_texts {
            starts.push(at);
            at += t.chars().count();
        }
        let prompt = Prompt {
            tools: 0,
            system: tools_text.chars().count(),
            messages: starts,
            total: at,
        };

        let mut b = Builder {
            nodes: Vec::new(),
            spans: Vec::new(),
            stack: Vec::new(),
        };
        if !system.is_empty() {
            let label = if system_unchanged {
                "System prompt (same as the previous call's)"
            } else {
                "System prompt"
            };
            b.leaf(
                label,
                lines(&system),
                Some((prompt.system, prompt.system + system.chars().count(), true)),
                system_unchanged,
            );
        }
        if let Some(tools) = call.tools.as_ref().and_then(Value::as_array) {
            let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
            b.leaf(
                format!("Tools ({})", names.len()),
                vec![names.join(", ")],
                Some((
                    prompt.tools,
                    prompt.tools + tools_text.chars().count(),
                    true,
                )),
                true,
            );
        }
        let mut names = std::collections::HashMap::new();
        for (i, m) in messages.iter().enumerate() {
            for block in m["content"].as_array().unwrap_or(&empty) {
                if block["type"] == "tool_use" {
                    names.insert(
                        block["id"].as_str().unwrap_or_default().to_owned(),
                        block["name"].as_str().unwrap_or("?").to_owned(),
                    );
                }
            }
            let start = prompt.messages[i];
            let end = start + message_texts[i].chars().count();
            if i == 0 {
                turn_message(&mut b, m, (start, end));
                if messages.len() > 1 {
                    b.open(format!("Earlier messages ({})", messages.len() - 1), false);
                }
                continue;
            }
            let role = m["role"].as_str().unwrap_or("?");
            let blocks = m["content"].as_array().unwrap_or(&empty);
            b.open(format!("{role}: {}", describe(blocks, &names)), true);
            share_blocks(&mut b, blocks, &names, (start, end), true);
            b.close();
        }
        b.stack.clear();
        let response = call.response["content"].as_array().unwrap_or(&empty);
        let stop = call.response["stop_reason"].as_str().unwrap_or("?");
        let response_chars: usize = response.iter().map(|r| r.to_string().chars().count()).sum();
        b.open(format!("Response (stop: {stop})"), false);
        share_blocks(&mut b, response, &names, (0, response_chars), false);
        b.close();
        let u = &call.usage;
        let mut usage = vec![
            format!("model {}", call.model),
            format!("input (uncached) {}", u.input_tokens),
            format!("cache write {}", u.cache_creation_input_tokens),
            format!("cache read {}", u.cache_read_input_tokens),
            format!("output {}", u.output_tokens),
            format!("latency {} ms", call.latency_ms),
            match call.cost_usd {
                Some(c) => format!("cost ${c:.4}"),
                None => "cost unknown (model not in the price table)".into(),
            },
        ];
        if let Some(e) = &call.effort {
            usage.insert(1, format!("effort {e}"));
        }
        b.leaf("Usage", usage, None, false);

        let mut tree = Tree { nodes: b.nodes };
        let prompt_tokens =
            u.input_tokens + u.cache_creation_input_tokens + u.cache_read_input_tokens;
        tree.share_tokens(
            &b.spans,
            (prompt.total, prompt_tokens, u.cache_read_input_tokens),
            (response_chars, u.output_tokens),
        );
        tree
    }

    /// Give each leaf its share of the tokens by its span, then total the
    /// containers (which have no span) from their children.
    fn share_tokens(
        &mut self,
        spans: &[Option<(usize, usize, bool)>],
        (prompt_chars, prompt_tokens, cache_read): (usize, u64, u64),
        (response_chars, output_tokens): (usize, u64),
    ) {
        let share = |chars: usize, of: usize, tokens: u64| -> u64 {
            if of == 0 {
                0
            } else {
                (tokens as f64 * chars as f64 / of as f64).round() as u64
            }
        };
        let cached_chars = if prompt_tokens == 0 {
            0
        } else {
            (prompt_chars as f64 * cache_read as f64 / prompt_tokens as f64).round() as usize
        };
        for (n, span) in self.nodes.iter_mut().zip(spans) {
            match *span {
                Some((start, end, true)) => {
                    n.tokens = share(end - start, prompt_chars, prompt_tokens);
                    let overlap = cached_chars.min(end).saturating_sub(start);
                    n.cached = share(overlap, prompt_chars, prompt_tokens).min(n.tokens);
                }
                Some((start, end, false)) => {
                    n.tokens = share(end - start, response_chars, output_tokens);
                }
                None => {}
            }
        }
        // Children come after their parents: total from the end.
        for i in (0..self.nodes.len()).rev() {
            if let Some(p) = self.nodes[i].parent {
                let (t, c) = (self.nodes[i].tokens, self.nodes[i].cached);
                self.nodes[p].tokens += t;
                self.nodes[p].cached += c;
            }
        }
    }

    /// Whether `node` can open or close.
    pub fn foldable(&self, node: usize) -> bool {
        !self.nodes[node].body.is_empty() || self.children(node).next().is_some()
    }

    fn children(&self, node: usize) -> impl Iterator<Item = usize> + '_ {
        (node + 1..self.nodes.len()).filter(move |&j| self.nodes[j].parent == Some(node))
    }

    pub fn toggle(&mut self, node: usize) {
        if self.foldable(node) {
            self.nodes[node].folded = !self.nodes[node].folded;
        }
    }

    /// Whether every ancestor of `node` is open.
    fn visible(&self, node: usize) -> bool {
        let mut at = self.nodes[node].parent;
        while let Some(p) = at {
            if self.nodes[p].folded {
                return false;
            }
            at = self.nodes[p].parent;
        }
        true
    }

    /// What is on screen, in order: each visible node's label, then its
    /// body if it is open.
    pub fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        for (i, n) in self.nodes.iter().enumerate() {
            if !self.visible(i) {
                continue;
            }
            rows.push(Row {
                node: i,
                line: None,
            });
            if !n.folded {
                rows.extend((0..n.body.len()).map(|l| Row {
                    node: i,
                    line: Some(l),
                }));
            }
        }
        rows
    }

    /// A node's label line without indentation or marker:
    /// `System prompt · 2.1k (100% cached)`.
    pub fn label(&self, node: usize) -> String {
        let n = &self.nodes[node];
        if !n.counted() {
            return n.label.clone();
        }
        format!("{} · {}", n.label, token_label(n))
    }

    /// `▸ ` or `▾ ` for a foldable node, else two spaces.
    pub fn marker(&self, node: usize) -> &'static str {
        match (self.foldable(node), self.nodes[node].folded) {
            (false, _) => "  ",
            (true, true) => "▸ ",
            (true, false) => "▾ ",
        }
    }

    /// Nodes whose label or body contains `query` (any case), in order.
    pub fn search(&self, query: &str) -> Vec<usize> {
        let q = query.to_lowercase();
        if q.is_empty() {
            return Vec::new();
        }
        (0..self.nodes.len())
            .filter(|&i| {
                let n = &self.nodes[i];
                n.label.to_lowercase().contains(&q)
                    || n.body.iter().any(|l| l.to_lowercase().contains(&q))
            })
            .collect()
    }

    /// Open every ancestor of `node`, and `node` itself when the match is
    /// in its body, so it shows.
    pub fn reveal(&mut self, node: usize, query: &str) {
        let mut at = self.nodes[node].parent;
        while let Some(p) = at {
            self.nodes[p].folded = false;
            at = self.nodes[p].parent;
        }
        let q = query.to_lowercase();
        let n = &mut self.nodes[node];
        if !n.label.to_lowercase().contains(&q)
            && n.body.iter().any(|l| l.to_lowercase().contains(&q))
        {
            n.folded = false;
        }
    }

    /// The whole tree as indented text, every node open: plain mode's
    /// `:context #seq.N`.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for (i, n) in self.nodes.iter().enumerate() {
            let pad = "  ".repeat(n.depth);
            writeln!(out, "{pad}{}", self.label(i)).unwrap();
            for line in &n.body {
                writeln!(out, "{pad}  │ {line}").unwrap();
            }
        }
        out.truncate(out.trim_end().len());
        out
    }
}

/// `2.1k (100% cached)`, `410`
pub fn token_label(n: &Node) -> String {
    if n.cached == 0 {
        tokens(n.tokens)
    } else {
        format!(
            "{} ({}% cached)",
            tokens(n.tokens),
            super::percent(n.cached, n.tokens)
        )
    }
}

/// The first message: the slice and turn message, a node per section.
fn turn_message(b: &mut Builder, m: &Value, (start, end): (usize, usize)) {
    let text: String = m["content"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    b.open("Turn message", false);
    let sections = sections(&text);
    let total: usize = sections.iter().map(|(_, l)| chars(l)).sum::<usize>().max(1);
    let mut at = start;
    let width = end - start;
    for (i, (label, body)) in sections.iter().enumerate() {
        let len = if i + 1 == sections.len() {
            end - at
        } else {
            width * chars(body) / total
        };
        b.leaf(
            label.clone(),
            body.clone(),
            Some((at, at + len, true)),
            true,
        );
        at += len;
    }
    b.close();
}

fn chars(lines: &[String]) -> usize {
    lines.iter().map(|l| l.chars().count() + 1).sum()
}

/// Split a turn message into labelled sections. A section starts at an
/// unindented line that looks like `Heading: …` (short, capitalised
/// heading, not a list item or a quoted input); the lines after it, up to
/// the next one, belong to it.
pub fn sections(text: &str) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for line in text.lines() {
        match heading(line) {
            Some(h) => out.push((h, vec![line.to_owned()])),
            None => match out.last_mut() {
                Some((_, body)) => body.push(line.to_owned()),
                None => out.push(("Message".into(), vec![line.to_owned()])),
            },
        }
    }
    out
}

fn heading(line: &str) -> Option<String> {
    if !line.starts_with(|c: char| c.is_ascii_uppercase()) {
        return None;
    }
    let (head, _) = line.split_once(':')?;
    // `Stats (check, adjust_stat)` → `Stats`; what the brackets say is
    // free text (`Glossary (lookup_terms; ? = undefined)`).
    let short = head.split(" (").next().unwrap_or(head).trim();
    let plain = !short.contains(['.', '!', '?', '"']);
    (plain && short.split_whitespace().count() <= 4).then(|| short.to_owned())
}

/// A message's blocks in a few words: `tool call look, tool call take`.
fn describe(blocks: &[Value], names: &std::collections::HashMap<String, String>) -> String {
    let parts: Vec<String> = blocks.iter().map(|b| block_label(b, names)).collect();
    if parts.is_empty() {
        "empty".into()
    } else {
        parts.join(", ")
    }
}

fn block_label(block: &Value, names: &std::collections::HashMap<String, String>) -> String {
    let name = |id: &Value| {
        names
            .get(id.as_str().unwrap_or_default())
            .cloned()
            .unwrap_or_else(|| "?".into())
    };
    match block["type"].as_str().unwrap_or("?") {
        "text" => "text".into(),
        "tool_use" => format!("tool call {}", block["name"].as_str().unwrap_or("?")),
        "tool_result" if block["is_error"] == json!(true) => {
            format!("tool result {} ✗ error", name(&block["tool_use_id"]))
        }
        "tool_result" => format!("tool result {}", name(&block["tool_use_id"])),
        "thinking" => "thinking".into(),
        "redacted_thinking" => "thinking (redacted)".into(),
        other => other.to_owned(),
    }
}

fn block_body(block: &Value) -> Vec<String> {
    match block["type"].as_str().unwrap_or("?") {
        "text" => lines(block["text"].as_str().unwrap_or_default()),
        "tool_use" => lines(&serde_json::to_string_pretty(&block["input"]).unwrap_or_default()),
        "tool_result" => match &block["content"] {
            Value::String(s) => lines(s),
            other => lines(&serde_json::to_string_pretty(other).unwrap_or_default()),
        },
        "thinking" => {
            let t = block["thinking"].as_str().unwrap_or_default();
            if t.trim().is_empty() {
                vec!["(not returned by the API)".into()]
            } else {
                lines(t)
            }
        }
        _ => lines(&serde_json::to_string_pretty(block).unwrap_or_default()),
    }
}

/// One leaf per block, sharing the characters `start..end` by each
/// block's JSON length.
fn share_blocks(
    b: &mut Builder,
    blocks: &[Value],
    names: &std::collections::HashMap<String, String>,
    (start, end): (usize, usize),
    in_prompt: bool,
) {
    let sizes: Vec<usize> = blocks
        .iter()
        .map(|x| x.to_string().chars().count())
        .collect();
    let total: usize = sizes.iter().sum::<usize>().max(1);
    let mut at = start;
    for (i, block) in blocks.iter().enumerate() {
        let len = if i + 1 == blocks.len() {
            end.saturating_sub(at)
        } else {
            (end - start) * sizes[i] / total
        };
        b.leaf(
            block_label(block, names),
            block_body(block),
            Some((at, at + len, in_prompt)),
            true,
        );
        at += len;
    }
}

fn lines(text: &str) -> Vec<String> {
    text.lines().map(str::to_owned).collect()
}

/// The call as one JSON document, system prompt and tools included: what
/// `y` copies, `e` opens and `--raw` prints.
pub fn raw_json(stored: &StoredCall) -> String {
    let c = &stored.call;
    let doc = json!({
        "id": stored.id,
        "turn_seq": stored.turn_seq,
        "reroll_id": stored.reroll_id,
        "variant": stored.variant,
        "role": c.role,
        "model": c.model,
        "effort": c.effort,
        "system": c.system,
        "tools": c.tools,
        "request": c.request,
        "response": c.response,
        "usage": c.usage,
        "latency_ms": c.latency_ms,
        "cost_usd": c.cost_usd,
        "created_at": stored.created_at,
    });
    serde_json::to_string_pretty(&doc).unwrap_or_default()
}

/// The banner over a reroll variant's call whose system prompt differs
/// from the original turn's call of the same role (prompts changed since
/// the turn was played). `None` for calls that aren't variants.
pub fn variant_banner(call: &StoredCall, original: Option<&StoredCall>) -> Option<String> {
    call.reroll_id?;
    let original = original?;
    (call.system_hash != original.system_hash).then(|| {
        format!(
            "This variant's {} system prompt differs from the original turn's \
             (the prompts have changed since it was played).",
            call.call.role
        )
    })
}
