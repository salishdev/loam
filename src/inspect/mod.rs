//! The REPL inspector: `:why`, `:relations`, `:quests`, `:inventory`,
//! `:stats`, `:time`, `:pressures`, `:log`, and the raw [`rules`] and
//! [`glossary`] listings (`:rules` and `:glossary` show the learned
//! mechanics in [`crate::knowledge`]).
//!
//! Every report is read from the world store and its event log. Nothing
//! here takes an LLM client, so inspecting never costs a call.

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::fmt::Write;

use serde_json::Value;

use crate::game;
use crate::glossary;
use crate::inventory::{DEFAULT_SLOTS, Inventory, InventoryError};
use crate::pressure;
use crate::quest;
use crate::rules::{self, Condition, RuleClass};
use crate::stats::{self, Capacity, Crossing, StatHook};
use crate::time;
use crate::world::{Cascaded, Entity, EntityId, Event, EventKind, Op, Relation, StoreError, World};

/// Longest value shown inline before it is clipped.
const VALUE_CHARS: usize = 100;
/// Most trail entries `:why` prints; older ones are counted, not shown.
const MAX_TRAIL: usize = 60;
/// Events `:log` lists when not given a count.
const LOG_DEFAULT: usize = 20;
/// Longest one-line summary `:log` prints for an event.
const LOG_LINE_CHARS: usize = 160;

/// Every rule in id order, retired ones marked.
pub fn rules(world: &World) -> Result<String, StoreError> {
    let all = rules::all(world)?;
    if all.is_empty() {
        return Ok("No rules yet.".into());
    }
    Ok(trim_end(all.iter().map(rule_text).collect()))
}

/// One rule in full: its heading, trigger, roles, conditions, effects and
/// outcome, a line each.
pub(crate) fn rule_text(r: &rules::Rule) -> String {
    let mut out = String::new();
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
    out
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

/// Every NPC entity with where it is and its props, then the bible's NPCs
/// that have no entity yet: world truth, not what the player knows, so no
/// Player-mode command shows it (`:npcs` opens the compendium's People). Bible NPCs match entities by name, ignoring
/// case.
pub fn npcs(world: &World) -> Result<String, StoreError> {
    let sightings = game::npcs(world)?;
    let bible = game::bible_npc_names(world)?;
    if sightings.is_empty() && bible.is_empty() {
        return Ok("No NPCs yet.".into());
    }
    let same = |a: &str, b: &str| a.trim().to_lowercase() == b.trim().to_lowercase();
    let mut out = String::new();
    for s in &sightings {
        let n = &s.npc;
        let in_bible = if bible.iter().any(|b| same(b, &n.name)) {
            " [bible]"
        } else {
            ""
        };
        let at = match s.room {
            Some(r) => {
                let name = world.entity(r)?.map(|e| e.name).unwrap_or_default();
                let with = if s.with_player {
                    "with the player, "
                } else {
                    ""
                };
                format!("{with}in #{r} {name}").trim_end().to_owned()
            }
            None => "nowhere".to_owned(),
        };
        writeln!(out, "#{} {}{in_bible}: {at}", n.id, n.name).unwrap();
        for (k, v) in &n.props {
            writeln!(out, "  {k} = {}", clip_value(v)).unwrap();
        }
    }
    let missing: Vec<&String> = bible
        .iter()
        .filter(|b| !sightings.iter().any(|s| same(b, &s.npc.name)))
        .collect();
    if !missing.is_empty() {
        writeln!(out, "In the bible, not yet in the world:").unwrap();
        for name in missing {
            writeln!(out, "  {name}").unwrap();
        }
    }
    Ok(trim_end(out))
}

/// The player's stat sheet, then counts of what the engine has done and
/// holds.
pub fn stats(world: &World) -> Result<String, StoreError> {
    let mut out = stat_sheet(world)?;
    writeln!(out).unwrap();
    out.push_str(&engine_counts(world)?);
    Ok(out)
}

/// Each stat with its value, bounds and stat rules: thresholds (marked
/// when the value is already on their firing side), capacity and growth.
fn stat_sheet(world: &World) -> Result<String, StoreError> {
    let sheet = stats::sheet(world)?;
    if sheet.is_empty() {
        return Ok("Stat sheet: none yet".into());
    }
    let rules = stats::stat_rules(world)?;
    let mut out = String::from("Stat sheet:");
    for s in sheet.values() {
        write!(out, "\n  {} {} [{}..{}]", s.name, s.value, s.min, s.max).unwrap();
        if !s.about.trim().is_empty() {
            write!(out, ": {}", s.about.trim()).unwrap();
        }
        for r in &rules {
            let Some(hook) = r.hook.as_ref().filter(|h| h.stat() == s.name) else {
                continue;
            };
            match hook {
                StatHook::Bounds { .. } => {}
                StatHook::Threshold { at, when, .. } => {
                    let reached = match when {
                        Crossing::FallsTo => s.value <= *at,
                        Crossing::RisesTo => s.value >= *at,
                    };
                    let mark = if reached { " (reached)" } else { "" };
                    write!(out, "\n    threshold #{} {}{mark}", r.id, r.def.trigger).unwrap();
                    if !r.def.outcome.trim().is_empty() {
                        write!(out, " => {}", clip(r.def.outcome.trim(), VALUE_CHARS)).unwrap();
                    }
                }
                StatHook::Capacity {
                    capacity,
                    base,
                    per_point,
                    ..
                } => {
                    let name = match capacity {
                        Capacity::CarryLimit => "carry limit",
                    };
                    let now = stats::capacity_at(*base, *per_point, s.value);
                    write!(out, "\n    #{} {} (now {name} {now})", r.id, r.def.trigger).unwrap();
                }
                StatHook::Advancement { step, .. } => {
                    write!(out, "\n    grows #{} {} (+{step})", r.id, r.def.trigger).unwrap();
                }
            }
        }
    }
    Ok(out)
}

/// Counts of what the engine has done and holds.
fn engine_counts(world: &World) -> Result<String, StoreError> {
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

    // Compendium entries are what the player knows, not world entities.
    let entities: Vec<Entity> = world
        .state()?
        .entities
        .into_iter()
        .filter(|e| !crate::world::is_entry(e))
        .collect();
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
    let id = match pick(world, query, ":why")? {
        Ok(id) => id,
        Err(reply) => return Ok(reply),
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
            for line in relation_lines(world, id, "  ")? {
                writeln!(out, "{line}").unwrap();
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

/// Every relation of an entity, both ways. `query` is as for [`why`].
pub fn relations(world: &World, query: &str) -> Result<String, StoreError> {
    let id = match pick(world, query, ":relations")? {
        Ok(id) => id,
        Err(reply) => return Ok(reply),
    };
    let Some(e) = world.entity(id)? else {
        return Ok(format!("#{id} no longer exists (or never did)."));
    };
    let mut out = format!("#{} {} ({})", e.id, e.name, e.kind);
    let lines = relation_lines(world, id, "  ")?;
    if lines.is_empty() {
        out.push_str("\n  no relations");
    }
    for line in lines {
        write!(out, "\n{line}").unwrap();
    }
    Ok(out)
}

/// `id`'s relations, outgoing (`-rel-> #o name`) then incoming
/// (`<-rel- #s name`), each with its props.
fn relation_lines(world: &World, id: EntityId, indent: &str) -> Result<Vec<String>, StoreError> {
    let name = |id: EntityId| -> Result<String, StoreError> {
        Ok(match world.entity(id)? {
            Some(e) => format!("#{id} {}", e.name),
            None => format!("#{id}"),
        })
    };
    let props = |r: &Relation| {
        if r.props.is_empty() {
            String::new()
        } else {
            format!(" {}", clip_value(&Value::Object(r.props.clone())))
        }
    };
    let mut lines = Vec::new();
    for r in world.relations_from(id, None)? {
        lines.push(format!(
            "{indent}-{}-> {}{}",
            r.rel,
            name(r.object)?,
            props(&r)
        ));
    }
    for r in world.relations_to(id, None)? {
        lines.push(format!(
            "{indent}<-{}- {}{}",
            r.rel,
            name(r.subject)?,
            props(&r)
        ));
    }
    Ok(lines)
}

/// Resolve an inspector query to one id, or the reply explaining why not.
fn pick(world: &World, query: &str, cmd: &str) -> Result<Result<EntityId, String>, StoreError> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(Err(format!("Usage: {cmd} <#id | name>")));
    }
    Ok(match find(world, query)? {
        Found::One(id) => Ok(id),
        Found::None => Err(format!("No entity matches {query:?}.")),
        Found::Many(candidates) => {
            let mut out = format!("{query:?} matches several entities; use an id:");
            for e in candidates {
                write!(out, "\n  #{} {} ({})", e.id, e.name, e.kind).unwrap();
            }
            Err(out)
        }
    })
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
    // Compendium entries carry the names of what they describe; they are
    // read through :compendium, not matched here.
    let entities: Vec<Entity> = world
        .state()?
        .entities
        .into_iter()
        .filter(|e| !crate::world::is_entry(e))
        .collect();
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
            let mut ops: Vec<String> = e
                .patch_touches()?
                .iter()
                .filter(|(_, ids)| ids.contains(&id))
                .map(|(op, ids)| render_op(op, created_ids(e, ids)))
                .collect();
            let cascaded: Vec<Cascaded> = match p.get("cascaded") {
                Some(c) => serde_json::from_value(c.clone()).map_err(|err| {
                    StoreError::Corrupt(format!("event {seq}: bad cascaded list: {err}"))
                })?,
                None => Vec::new(),
            };
            ops.extend(
                cascaded
                    .iter()
                    .map(|c| &c.relation)
                    .filter(|r| r.subject == id || r.object == id)
                    .map(|r| format!("cascade removed #{} {} #{}", r.subject, r.rel, r.object)),
            );
            if ops.is_empty() && rule_id != Some(id) {
                return Ok(None);
            }
            let by = patch_by(p);
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

/// The ids to hand [`render_op`] for an op of `e`: a rejected patch created
/// nothing, so its creates get none (the first id there is the location).
fn created_ids<'a>(e: &Event, ids: &'a [EntityId]) -> &'a [EntityId] {
    if e.kind == EventKind::PatchRejected {
        &[]
    } else {
        ids
    }
}

/// Who applied a patch: the engine, a rule (with version and bindings) or
/// the director.
fn patch_by(p: &Value) -> String {
    if p["by"] == "engine" {
        "engine".to_owned()
    } else if let Some(r) = p["rule"]["id"].as_i64() {
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
    }
}

/// `:log`: the event log.
///
/// - `:log` lists the last 20 events, one line each.
/// - A number lists that many; `all` lists every one.
/// - Kind names (`patch`, `roll`, `tool_call`, …) keep only those kinds.
/// - `#<seq>` shows that one event in full.
///
/// Words combine in any order: `:log patch roll 50`.
/// `:inventory`: what the player carries, bags' contents indented under
/// them, equipped slots, and load against the carry limit.
pub fn inventory(world: &World) -> Result<String, StoreError> {
    let Some(player) = game::player_id(world)? else {
        return Ok("No player yet.".into());
    };
    let listing = match Inventory::new(DEFAULT_SLOTS, stats::carry_limit).list(world, player) {
        Ok(l) => l,
        Err(InventoryError::Store(e)) => return Err(e),
        Err(InventoryError::Refused(reason)) => return Ok(format!("No inventory: {reason}.")),
    };
    if listing.items.is_empty() {
        return Ok(format!("Carrying nothing (limit {}).", listing.limit));
    }
    let mut out = format!("Carrying {} of {} bulk", listing.load, listing.limit);
    if listing.over_limit {
        out.push_str(" (over the limit)");
    }
    out.push(':');
    // Items come depth-first, so a bag is listed before what's in it.
    let mut depth: BTreeMap<EntityId, usize> = BTreeMap::new();
    for item in &listing.items {
        let d = item.inside.and_then(|c| depth.get(&c)).map_or(1, |d| d + 1);
        depth.insert(item.id, d);
        write!(out, "\n{}#{} {}", "  ".repeat(d), item.id, item.name).unwrap();
        if let Some(slot) = &item.equipped {
            write!(out, " (equipped: {slot})").unwrap();
        }
    }
    Ok(out)
}

/// Advances `:time` lists under the clock.
const TIME_ADVANCES: usize = 8;

/// `:time`: the world clock and its latest advances, newest first.
pub fn time(world: &World) -> Result<String, StoreError> {
    let mut out = format!("It is {}.\n", time::now(world)?);
    let recent = time::recent(world, TIME_ADVANCES)?;
    if recent.is_empty() {
        out.push_str("No time has passed yet.\n");
    } else {
        out.push_str("Recent turns:\n");
        for a in recent {
            writeln!(
                out,
                "  #{} +{} ({}) -> {}",
                a.seq,
                time::span(a.minutes),
                a.source,
                a.to
            )
            .unwrap();
        }
    }
    Ok(trim_end(out))
}

/// `:pressures`: active pressures nearest first, then resolved ones in
/// the order started, each with its due time, urgency, concerns and
/// history.
pub fn pressures(world: &World) -> Result<String, StoreError> {
    let all = pressure::pressures(world)?;
    if all.is_empty() {
        return Ok("No pressures yet.".into());
    }
    let now = time::now(world)?;
    let mut out = format!("It is {now}.\n");
    let active = pressure::active(world)?;
    let resolved = all
        .iter()
        .filter(|p| p.status == pressure::Status::Resolved);
    for p in active.iter().chain(resolved) {
        let tag = match p.status {
            pressure::Status::Active => p.urgency(now).as_str(),
            pressure::Status::Resolved => "resolved",
        };
        writeln!(out, "#{} {} [{tag}]", p.id, p.what).unwrap();
        writeln!(out, "  then: {}", p.consequence).unwrap();
        writeln!(out, "  started {}, {}", p.started, p.due_text(now)).unwrap();
        if !p.concerns.is_empty() {
            let names: Vec<String> = p
                .concerns
                .iter()
                .map(|&id| -> Result<String, StoreError> {
                    Ok(match world.entity(id)? {
                        Some(e) => format!("#{id} {}", e.name),
                        None => format!("#{id}"),
                    })
                })
                .collect::<Result<_, _>>()?;
            writeln!(out, "  concerns {}", names.join(", ")).unwrap();
        }
        for a in &p.adjustments {
            let way = if a.minutes >= 0 {
                "delayed"
            } else {
                "hastened"
            };
            writeln!(
                out,
                "  - {} {way} {}: {}",
                time::Moment(a.at),
                time::span(a.minutes.unsigned_abs()),
                a.reason
            )
            .unwrap();
        }
        if let Some(r) = &p.resolution {
            writeln!(out, "  => {r}").unwrap();
        }
    }
    Ok(trim_end(out))
}

pub fn log(world: &World, arg: &str) -> Result<String, StoreError> {
    let usage = || {
        let kinds: Vec<&str> = EventKind::ALL.iter().map(|k| k.as_str()).collect();
        format!(
            "Usage: :log [count | all] [kind…] or :log #<event>. Kinds: {}.",
            kinds.join(", ")
        )
    };
    if let Some(seq) = arg.strip_prefix('#') {
        let Ok(seq) = seq.trim().parse::<i64>() else {
            return Ok(usage());
        };
        return Ok(match world.event(seq)? {
            Some(e) => event_detail(&e)?,
            None => format!("No event #{seq}."),
        });
    }

    let mut limit = Some(LOG_DEFAULT);
    let mut kinds = Vec::new();
    for word in arg.split_whitespace() {
        if word == "all" {
            limit = None;
        } else if let Ok(n) = word.parse::<usize>() {
            limit = Some(n);
        } else if let Some(k) = EventKind::parse(word) {
            kinds.push(k);
        } else {
            return Ok(format!("Unknown :log word {word:?}. {}", usage()));
        }
    }

    let events: Vec<Event> = world
        .events()?
        .into_iter()
        .filter(|e| kinds.is_empty() || kinds.contains(&e.kind))
        .collect();
    if events.is_empty() {
        return Ok("No events logged.".into());
    }
    let skipped = limit.map_or(0, |n| events.len().saturating_sub(n));
    let mut out = String::new();
    if skipped > 0 {
        writeln!(
            out,
            "({skipped} earlier event(s) not shown; :log all lists them)"
        )
        .unwrap();
    }
    for e in &events[skipped..] {
        writeln!(out, "{}", event_line(e)?).unwrap();
    }
    Ok(trim_end(out))
}

/// One event on one line: its number, kind and a clipped summary.
fn event_line(e: &Event) -> Result<String, StoreError> {
    let summary = event_summary(e)?;
    Ok(format!(
        "#{} {}: {}",
        e.seq,
        e.kind.as_str(),
        clip(&summary.replace('\n', " "), LOG_LINE_CHARS)
    ))
}

/// What an event records, in words.
fn event_summary(e: &Event) -> Result<String, StoreError> {
    let p = &e.payload;
    let text = |key: &str| p[key].as_str().unwrap_or_default().trim().to_owned();
    Ok(match e.kind {
        EventKind::Input if p.get("seed").is_some() => format!("seed: {}", text("text")),
        EventKind::Input => format!("> {}", text("text")),
        EventKind::Narration => text("text"),
        EventKind::Patch | EventKind::PatchRejected => {
            let ops: Vec<String> = e
                .patch_touches()?
                .iter()
                .map(|(op, ids)| render_op(op, created_ids(e, ids)))
                .collect();
            if e.kind == EventKind::PatchRejected {
                format!(
                    "by {} (op {}: {}): {}",
                    patch_by(p),
                    p["op"],
                    text("reason"),
                    ops.join("; ")
                )
            } else {
                format!("by {}: {}", patch_by(p), ops.join("; "))
            }
        }
        EventKind::ToolCall => {
            let mut s = format!("{} {}", text("name"), p["input"]);
            if p["is_error"] == true {
                write!(s, " ERROR: {}", text("result")).unwrap();
            }
            s
        }
        EventKind::RuleChange => {
            let mut s = format!(
                "rule #{} {:?} {} v{}",
                p["id"],
                p["rule"]["name"].as_str().unwrap_or("?"),
                p["action"].as_str().unwrap_or("changed"),
                p["version"]
            );
            if let Some(reason) = p["reason"].as_str() {
                write!(s, ": {reason}").unwrap();
            }
            s
        }
        EventKind::Roll => format!(
            "{} d20 {} + {} = {} vs {}: {} ({})",
            text("stat"),
            p["die"],
            p["value"],
            p["total"],
            p["difficulty"],
            text("result"),
            text("reason")
        ),
        EventKind::Judgment if p["by"] == "interpreter" => {
            let rule = p["rule"]["id"]
                .as_i64()
                .map(|r| format!(" rule #{r}"))
                .unwrap_or_default();
            format!("interpreter: {}{rule}", text("verdict"))
        }
        EventKind::Judgment => format!(
            "quest #{} {}: {}",
            p["quest"],
            text("verdict"),
            text("reason")
        ),
        EventKind::Tick => text("summary"),
        EventKind::Time => format!(
            "+{} ({}) -> {}",
            time::span(p["minutes"].as_u64().unwrap_or(0)),
            text("source"),
            time::Moment(p["minute"].as_u64().unwrap_or(0))
        ),
    })
}

/// One event in full: when it was logged, a summary (prose whole) and its
/// payload.
fn event_detail(e: &Event) -> Result<String, StoreError> {
    let mut out = format!("#{} {} at {}\n", e.seq, e.kind.as_str(), e.at);
    let mut payload = e.payload.clone();
    if matches!(e.kind, EventKind::Input | EventKind::Narration) {
        // Prose is shown whole, as written, and left out of the payload.
        writeln!(out, "{}", event_summary(e)?).unwrap();
        if let Some(obj) = payload.as_object_mut() {
            obj.remove("text");
        }
    } else {
        // The payload has it all; the summary is only a reading aid.
        let summary = event_summary(e)?.replace('\n', " ");
        writeln!(out, "{}", clip(&summary, LOG_LINE_CHARS)).unwrap();
    }
    if payload.as_object().is_none_or(|o| !o.is_empty()) {
        out.push('\n');
        out.push_str(&serde_json::to_string_pretty(&payload).unwrap());
    }
    Ok(trim_end(out))
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
        Condition::Entity { role, id: Some(id) } => format!("{role} is #{id}"),
        Condition::Entity { role, id: None } => format!("{role} is #? (pinned entity gone)"),
        Condition::Related { role, rel, other } => format!("{role} {rel} {other}"),
        Condition::NotRelated { role, rel, other } => format!("{role} not {rel} {other}"),
    }
}

/// One op in words. `ids` are the entities it touched, as from
/// [`Event::patch_touches`]; a create shows the id it got.
pub(crate) fn render_op(op: &Op, ids: &[EntityId]) -> String {
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
        Op::Relate {
            subject,
            rel,
            object,
            props,
        } => {
            let mut s = format!("relate {subject} {rel} {object}");
            if !props.is_empty() {
                write!(s, " {}", clip_value(&Value::Object(props.clone()))).unwrap();
            }
            s
        }
        Op::Unrelate {
            subject,
            rel,
            object,
        } => format!("unrelate {subject} {rel} {object}"),
    }
}

pub(crate) fn clip_value(v: &Value) -> String {
    clip(&v.to_string(), VALUE_CHARS)
}

pub(crate) fn clip(s: &str, max: usize) -> String {
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
