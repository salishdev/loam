//! The REPL inspector: `:rules`, `:glossary`, `:why`, `:quests`, `:stats`.
//!
//! Every report is read from the world store and its event log. Nothing
//! here takes an LLM client, so inspecting never costs a call.

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::fmt::Write;

use serde_json::Value;

use crate::glossary;
use crate::quest;
use crate::rules::{self, Condition, RuleClass};
use crate::world::{Entity, EntityId, Event, EventKind, Op, StoreError, World};

/// Longest value shown inline before it is clipped.
const VALUE_CHARS: usize = 100;
/// Most trail entries `:why` prints; older ones are counted, not shown.
const MAX_TRAIL: usize = 60;

/// Every rule in id order, retired ones marked.
pub fn rules(world: &World) -> Result<String, StoreError> {
    let all = rules::all(world)?;
    if all.is_empty() {
        return Ok("No rules yet.".into());
    }
    let mut out = String::new();
    for r in all {
        let class = match r.class {
            RuleClass::World => "world",
            RuleClass::Stat => "stat",
        };
        let retired = if r.is_active() { "" } else { " RETIRED" };
        writeln!(
            out,
            "#{} {} [{class} v{}]{retired}",
            r.id, r.def.name, r.version
        )
        .unwrap();
        writeln!(out, "  when: {}", r.def.trigger).unwrap();
        for (role, what) in &r.def.roles {
            writeln!(out, "  role {role}: {what}").unwrap();
        }
        for c in &r.def.conditions {
            writeln!(out, "  if {}", condition(c)).unwrap();
        }
        for op in &r.def.effects {
            writeln!(out, "  do {}", render_op(op, &[])).unwrap();
        }
        if !r.def.outcome.trim().is_empty() {
            writeln!(out, "  => {}", r.def.outcome.trim()).unwrap();
        }
    }
    Ok(trim_end(out))
}

/// Every glossary term, sorted by name.
pub fn glossary(world: &World) -> Result<String, StoreError> {
    let mut out = String::new();
    for (name, term) in glossary::terms(world)? {
        let meaning = if term.meaning.trim().is_empty() {
            "(undefined)"
        } else {
            term.meaning.trim()
        };
        let free = if term.free_text { " [free text]" } else { "" };
        writeln!(out, "{name}{free}: {meaning}").unwrap();
        for (value, meaning) in &term.values {
            if meaning.trim().is_empty() {
                writeln!(out, "  = {value}").unwrap();
            } else {
                writeln!(out, "  = {value}: {}", meaning.trim()).unwrap();
            }
        }
    }
    Ok(trim_end(out))
}

/// Every quest, in the order spawned.
pub fn quests(world: &World) -> Result<String, StoreError> {
    let quests = quest::quests(world)?;
    if quests.is_empty() {
        return Ok("No quests yet.".into());
    }
    let mut out = String::new();
    for q in quests {
        writeln!(out, "#{} {} [{}]", q.id, q.title, q.status.as_str()).unwrap();
        writeln!(out, "  {}", q.goal.as_deref().unwrap_or(&q.hook)).unwrap();
        for d in &q.developments {
            writeln!(out, "  - {d}").unwrap();
        }
        if let Some(r) = &q.resolution {
            writeln!(out, "  => {r}").unwrap();
        }
    }
    Ok(trim_end(out))
}

/// Counts of what the engine has done and holds.
pub fn stats(world: &World) -> Result<String, StoreError> {
    let events = world.events()?;
    let mut by_kind: BTreeMap<&str, usize> = BTreeMap::new();
    let (mut turns, mut fast, mut escalated, mut tool_errors) = (0, 0, 0, 0);
    for e in &events {
        *by_kind.entry(e.kind.as_str()).or_default() += 1;
        match e.kind {
            EventKind::Input if e.payload.get("seed").is_none() => turns += 1,
            EventKind::Judgment if e.payload["by"] == "interpreter" => {
                match e.payload["verdict"].as_str() {
                    Some("fast_path") => fast += 1,
                    _ => escalated += 1,
                }
            }
            EventKind::ToolCall if e.payload["is_error"] == true => tool_errors += 1,
            _ => {}
        }
    }
    let count = |k: EventKind| by_kind.get(k.as_str()).copied().unwrap_or(0);

    let mut out = String::new();
    write!(out, "Events: {}", world.last_seq()?).unwrap();
    if let Some(at) = world.forked_at()? {
        write!(out, " (branch forked at #{at})").unwrap();
    }
    writeln!(out).unwrap();
    for (kind, n) in &by_kind {
        writeln!(out, "  {kind}: {n}").unwrap();
    }
    writeln!(
        out,
        "Turns: {turns} ({fast} fast path, {escalated} escalated by the interpreter)"
    )
    .unwrap();
    writeln!(
        out,
        "Tool calls: {} ({tool_errors} errors)",
        count(EventKind::ToolCall)
    )
    .unwrap();
    writeln!(
        out,
        "Patches: {} applied, {} rejected",
        count(EventKind::Patch),
        count(EventKind::PatchRejected)
    )
    .unwrap();
    writeln!(out, "World ticks: {}", count(EventKind::Tick)).unwrap();

    let all_rules = rules::all(world)?;
    let tally = |class: RuleClass| {
        let of: Vec<_> = all_rules.iter().filter(|r| r.class == class).collect();
        let active = of.iter().filter(|r| r.is_active()).count();
        (active, of.len() - active)
    };
    let (wa, wr) = tally(RuleClass::World);
    let (sa, _) = tally(RuleClass::Stat);
    writeln!(
        out,
        "Rules: {} ({wa} world active, {wr} world retired, {sa} stat)",
        all_rules.len()
    )
    .unwrap();

    let terms = glossary::terms(world)?;
    let undefined = terms
        .values()
        .filter(|t| t.meaning.trim().is_empty())
        .count();
    writeln!(
        out,
        "Glossary: {} terms ({undefined} undefined)",
        terms.len()
    )
    .unwrap();

    let quests = quest::quests(world)?;
    let mut by_status: BTreeMap<&str, usize> = BTreeMap::new();
    for q in &quests {
        *by_status.entry(q.status.as_str()).or_default() += 1;
    }
    writeln!(out, "Quests: {}{}", quests.len(), tally_list(&by_status)).unwrap();

    let entities = world.state()?.entities;
    let mut by_ekind: BTreeMap<&str, usize> = BTreeMap::new();
    for e in &entities {
        *by_ekind.entry(e.kind.as_str()).or_default() += 1;
    }
    write!(out, "Entities: {}{}", entities.len(), tally_list(&by_ekind)).unwrap();
    Ok(out)
}

/// " (a 1, b 2)", or nothing when empty.
fn tally_list(counts: &BTreeMap<&str, usize>) -> String {
    if counts.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = counts.iter().map(|(k, n)| format!("{k} {n}")).collect();
    format!(" ({})", parts.join(", "))
}

/// An entity's current state and the trail of logged events that touched
/// it, grouped under the player input that led to each. `query` is `#12`,
/// `12` or a name (exact, then a unique substring; case-insensitive).
pub fn why(world: &World, query: &str) -> Result<String, StoreError> {
    let query = query.trim();
    if query.is_empty() {
        return Ok("Usage: :why <#id | name>".into());
    }
    let id = match find(world, query)? {
        Found::One(id) => id,
        Found::None => return Ok(format!("No entity matches {query:?}.")),
        Found::Many(candidates) => {
            let mut out = format!("{query:?} matches several entities; use an id:");
            for e in candidates {
                write!(out, "\n  #{} {} ({})", e.id, e.name, e.kind).unwrap();
            }
            return Ok(out);
        }
    };

    let mut out = String::new();
    match world.entity(id)? {
        Some(e) => {
            let at = match e.location {
                Some(loc) => match world.entity(loc)? {
                    Some(l) => format!("in #{loc} {}", l.name),
                    None => format!("in #{loc}"),
                },
                None => "at the top level".into(),
            };
            writeln!(out, "#{} {} ({}) {at}", e.id, e.name, e.kind).unwrap();
            for (k, v) in &e.props {
                writeln!(out, "  {k} = {}", clip_value(v)).unwrap();
            }
        }
        None => writeln!(out, "#{id} no longer exists (or never did).").unwrap(),
    }

    // Each entry is (group header, line); the header is the input that led
    // to it, printed once per run of entries.
    let mut entries: Vec<(String, String)> = Vec::new();
    let mut context = "[before play]".to_owned();
    for e in world.events()? {
        match e.kind {
            EventKind::Input => {
                let text = e.payload["text"].as_str().unwrap_or_default();
                context = if e.payload.get("seed").is_some() {
                    format!("[seed, #{}]", e.seq)
                } else {
                    format!("> {} [#{}]", clip(text, VALUE_CHARS), e.seq)
                };
                continue;
            }
            EventKind::Narration => {
                context = "[between turns]".into();
                continue;
            }
            _ => {}
        }
        if let Some(line) = trail_line(&e, id)? {
            entries.push((context.clone(), line));
        }
    }

    if entries.is_empty() {
        write!(out, "No logged event touches #{id}.").unwrap();
        return Ok(out);
    }
    let skipped = entries.len().saturating_sub(MAX_TRAIL);
    if skipped > 0 {
        writeln!(out, "({skipped} earlier event(s) not shown)").unwrap();
    }
    let mut last: Option<&str> = None;
    for (header, line) in &entries[skipped..] {
        if last != Some(header.as_str()) {
            writeln!(out, "{header}").unwrap();
            last = Some(header);
        }
        writeln!(out, "  {line}").unwrap();
    }
    Ok(trim_end(out))
}

enum Found {
    None,
    One(EntityId),
    Many(Vec<Entity>),
}

fn find(world: &World, query: &str) -> Result<Found, StoreError> {
    if let Ok(id) = query.trim_start_matches('#').parse::<EntityId>() {
        return Ok(Found::One(id));
    }
    let q = query.to_lowercase();
    let entities = world.state()?.entities;
    let pick = |matches: Vec<&Entity>| match matches.len() {
        0 => None,
        1 => Some(Found::One(matches[0].id)),
        _ => Some(Found::Many(matches.into_iter().cloned().collect())),
    };
    let exact = entities
        .iter()
        .filter(|e| e.name.to_lowercase() == q)
        .collect();
    if let Some(f) = pick(exact) {
        return Ok(f);
    }
    let partial = entities
        .iter()
        .filter(|e| e.name.to_lowercase().contains(&q))
        .collect();
    Ok(pick(partial).unwrap_or(Found::None))
}

/// One trail line if `event` concerns entity `id`.
fn trail_line(e: &Event, id: EntityId) -> Result<Option<String>, StoreError> {
    let p = &e.payload;
    let seq = e.seq;
    Ok(match e.kind {
        EventKind::Patch | EventKind::PatchRejected => {
            let rule_id = p["rule"]["id"].as_i64();
            let ops: Vec<String> = e
                .patch_touches()?
                .iter()
                .filter(|(_, ids)| ids.contains(&id))
                .map(|(op, ids)| render_op(op, ids))
                .collect();
            if ops.is_empty() && rule_id != Some(id) {
                return Ok(None);
            }
            let by = if p["by"] == "engine" {
                "engine".to_owned()
            } else if let Some(r) = rule_id {
                let v = p["rule"]["version"].as_u64().unwrap_or(0);
                let bindings = p["bindings"]
                    .as_object()
                    .map(|b| {
                        b.iter()
                            .map(|(role, v)| format!("{role}=#{v}"))
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default();
                format!("rule #{r} v{v} {bindings}").trim_end().to_owned()
            } else {
                "director".to_owned()
            };
            let what = if ops.is_empty() {
                "applied (does not touch this entity)".to_owned()
            } else {
                ops.join("; ")
            };
            if e.kind == EventKind::Patch {
                Some(format!("#{seq} patch by {by}: {what}"))
            } else {
                let reason = p["reason"].as_str().unwrap_or_default();
                Some(format!(
                    "#{seq} REJECTED patch by {by}: {what} (op {}: {reason})",
                    p["op"]
                ))
            }
        }
        EventKind::RuleChange if p["id"].as_i64() == Some(id) => {
            let action = p["action"].as_str().unwrap_or("changed");
            let mut line = format!("#{seq} rule {action} v{}", p["version"]);
            if let Some(reason) = p["reason"].as_str() {
                write!(line, ": {}", clip(reason, VALUE_CHARS)).unwrap();
            }
            Some(line)
        }
        EventKind::Judgment => {
            let named = p["quest"].as_i64() == Some(id)
                || p["rule"]["id"].as_i64() == Some(id)
                || contains_id(&p["touched"], id)
                || p["bindings"]
                    .as_object()
                    .is_some_and(|b| b.values().any(|v| v.as_i64() == Some(id)));
            if !named {
                return Ok(None);
            }
            let verdict = p["verdict"].as_str().unwrap_or("?");
            if p["by"] == "interpreter" {
                let rule = p["rule"]["id"]
                    .as_i64()
                    .map(|r| format!(" rule #{r}"))
                    .unwrap_or_default();
                Some(format!("#{seq} interpreter: {verdict}{rule}"))
            } else {
                let reason = p["reason"].as_str().unwrap_or_default();
                Some(format!(
                    "#{seq} quest #{} judged {verdict}: {}",
                    p["quest"],
                    clip(reason, VALUE_CHARS)
                ))
            }
        }
        EventKind::Tick => {
            let touched = p["room"].as_i64() == Some(id)
                || p["changes"]
                    .as_array()
                    .is_some_and(|cs| cs.iter().any(|c| contains_id(&c["rooms"], id)));
            if !touched {
                return Ok(None);
            }
            let summary = p["summary"].as_str().unwrap_or_default();
            Some(format!("#{seq} world tick: {}", clip(summary, VALUE_CHARS)))
        }
        _ => None,
    })
}

fn contains_id(v: &Value, id: EntityId) -> bool {
    v.as_array()
        .is_some_and(|a| a.iter().any(|x| x.as_i64() == Some(id)))
}

fn condition(c: &Condition) -> String {
    match c {
        Condition::Kind { role, is } => format!("{role} is a {is}"),
        Condition::Prop { role, prop, is } => match is {
            Some(v) => format!("{role}.{prop} = {v}"),
            None => format!("{role} has {prop}"),
        },
        Condition::NotProp { role, prop, is } => match is {
            Some(v) => format!("{role}.{prop} != {v}"),
            None => format!("{role} lacks {prop}"),
        },
        Condition::Carried { role } => format!("player carries {role}"),
        Condition::Entity { role, id } => format!("{role} is #{id}"),
    }
}

/// One op in words. `ids` are the entities it touched, as from
/// [`Event::patch_touches`]; a create shows the id it got.
fn render_op(op: &Op, ids: &[EntityId]) -> String {
    match op {
        Op::Create {
            name,
            kind,
            location,
            props,
            ..
        } => {
            let mut s = match ids.first() {
                Some(id) => format!("create #{id} {name:?} ({kind})"),
                None => format!("create {name:?} ({kind})"),
            };
            if let Some(loc) = location {
                write!(s, " in {loc}").unwrap();
            }
            for (k, v) in props {
                write!(s, ", {k} = {}", clip_value(v)).unwrap();
            }
            s
        }
        Op::Move { entity, to } => match to {
            Some(to) => format!("move {entity} into {to}"),
            None => format!("move {entity} to the top level"),
        },
        Op::SetProp { entity, key, value } => {
            format!("set {entity}.{key} = {}", clip_value(value))
        }
        Op::RemoveProp { entity, key } => format!("remove {entity}.{key}"),
        Op::Rename { entity, name } => format!("rename {entity} to {name:?}"),
        Op::Destroy { entity } => format!("destroy {entity}"),
    }
}

fn clip_value(v: &Value) -> String {
    clip(&v.to_string(), VALUE_CHARS)
}

fn clip(s: &str, max: usize) -> String {
    let s = s.trim();
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_owned(),
    }
}

fn trim_end(mut s: String) -> String {
    s.truncate(s.trim_end().len());
    s
}
