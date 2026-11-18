//! What both front ends (the TUI and the plain REPL) share about `:`
//! commands and engine notices.
//!
//! Inspector commands read only the store, through whatever [`World`] they
//! are given (the front ends pass their read-only view), never a model.

use crate::game::{GameError, TickOutcome, TurnOutcome};
use crate::inspect;
use crate::quest::Verdict;
use crate::world::{EventKind, StoreError, StoredCall, World};
use crate::xray;

/// Every `:` command, for Tab completion and help.
pub const COMMANDS: &[&str] = &[
    ":brain",
    ":context",
    ":fork",
    ":glossary",
    ":help",
    ":i",
    ":inventory",
    ":log",
    ":mouse",
    ":npcs",
    ":panel",
    ":pressures",
    ":quests",
    ":quit",
    ":relations",
    ":rules",
    ":stats",
    ":tick",
    ":time",
    ":why",
    ":xray",
];

/// The command list `:help` prints.
pub const HELP: &str = "Commands: :rules, :glossary, :why <#id|name>, :relations <#id|name>, \
:quests, :npcs, :inventory (:i), :stats, :time, :pressures, :log [count|all] [kind…] | \
:log #<event>, :fork <event> <path>, :tick, :quit

Workbench (see and steer the models): :xray [on|off] shows each turn's calls, cache use, \
cost, refusals, rules and rolls; :context [#turn] lists a turn's calls, :context #turn.N \
[--raw] shows one call's full context (--raw: its JSON); :brain [<role>=<model>] \
[effort=<level>] | :brain reset shows or changes each role's model and effort.";

/// The commands that start with `prefix` (which starts with `:`), sorted.
pub fn complete(prefix: &str) -> Vec<&'static str> {
    COMMANDS
        .iter()
        .copied()
        .filter(|c| c.starts_with(prefix))
        .collect()
}

/// Splits `:cmd rest` into the command and its trimmed argument.
pub fn split(input: &str) -> (&str, &str) {
    let (cmd, arg) = input.split_once(char::is_whitespace).unwrap_or((input, ""));
    (cmd, arg.trim())
}

/// The text of a report command (an inspector, `:fork`), or `None` when
/// `cmd` isn't one. `Err` holds an engine notice: the report failed and the
/// save is still usable.
pub fn report(world: &World, cmd: &str, arg: &str) -> Option<Result<String, String>> {
    let result = match cmd {
        ":rules" => inspect::rules(world),
        ":glossary" => inspect::glossary(world),
        ":why" => inspect::why(world, arg),
        ":quests" => inspect::quests(world),
        ":npcs" => inspect::npcs(world),
        ":inventory" | ":i" => inspect::inventory(world),
        ":relations" => inspect::relations(world, arg),
        ":stats" => inspect::stats(world),
        ":time" => inspect::time(world),
        ":pressures" => inspect::pressures(world),
        ":log" => inspect::log(world, arg),
        ":fork" => return Some(fork(world, arg)),
        _ => return None,
    };
    Some(result.map_err(|e| format!("[{cmd} unavailable: {e}]")))
}

/// `:fork <event> <path>`: branch the session at an event into a new save.
/// `Ok` is what to tell the player (including usage mistakes); `Err` is an
/// engine notice.
pub fn fork(world: &World, arg: &str) -> Result<String, String> {
    let mut parts = arg.split_whitespace();
    let (Some(Ok(at)), Some(path), None) = (
        parts
            .next()
            .map(|n| n.trim_start_matches('#').parse::<i64>()),
        parts.next(),
        parts.next(),
    ) else {
        return Ok("Usage: :fork <event> <path>".into());
    };
    // Checked first so a bad event number doesn't leave an empty save behind.
    match world.last_seq() {
        Ok(last) if (0..=last).contains(&at) => {}
        Ok(last) => return Ok(format!("No event #{at}: the log ends at #{last}.")),
        Err(e) => return Err(format!("[fork failed: {e}]")),
    }
    match world.fork(at, path) {
        Ok(_) => Ok(format!(
            "Forked at event #{at} into {path}; run `loam {path}` to play it."
        )),
        Err(e) => Err(format!("[fork failed: {e}]")),
    }
}

/// `:xray [on|off]`: the X-ray's new state (no argument toggles it), or
/// how to use the command.
pub fn xray_switch(arg: &str, on: bool) -> Result<bool, &'static str> {
    match arg {
        "" => Ok(!on),
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err("Usage: :xray [on|off]"),
    }
}

/// Whether the X-ray starts on in plain mode: `LOAM_XRAY=1`.
pub fn xray_from_env(value: Option<&str>) -> bool {
    value.is_some_and(|v| v.trim() == "1")
}

/// Plain mode's footer after a turn: the X-ray of the latest turn, if
/// X-ray is `on` and a turn was played after `before` (the latest turn
/// before the command ran).
pub fn xray_footer(world: &World, before: Option<i64>, on: bool) -> Option<String> {
    if !on {
        return None;
    }
    let seq = xray::latest_turn(world).ok().flatten()?;
    if before.is_some_and(|b| b >= seq) {
        return None;
    }
    xray::turn(world, seq)
        .ok()
        .flatten()
        .map(|x| xray::footer(&x))
}

/// What `:context` asked for.
#[derive(Debug, Clone, PartialEq)]
pub enum ContextReport {
    /// A turn's call list, or what to tell the player instead.
    Text(String),
    /// Call `n` of the turn whose input is event `seq`.
    Call {
        seq: i64,
        n: usize,
        call: Box<StoredCall>,
        raw: bool,
    },
}

/// `:context [#seq | #seq.N] [--raw]`: the latest turn's (or turn `seq`'s)
/// calls, or one call. `Err` is an engine notice.
pub fn context(world: &World, arg: &str) -> Result<ContextReport, String> {
    let failed = |e: StoreError| format!("[:context unavailable: {e}]");
    let usage = || ContextReport::Text("Usage: :context [#turn | #turn.N] [--raw]".into());
    let mut raw = false;
    let mut target = None;
    for word in arg.split_whitespace() {
        match word {
            "--raw" => raw = true,
            _ if target.is_none() => target = Some(word),
            _ => return Ok(usage()),
        }
    }
    let (seq, n) = match target {
        None => match xray::latest_turn(world).map_err(failed)? {
            Some(seq) => (seq, None),
            None => return Ok(ContextReport::Text("No turns played yet.".into())),
        },
        Some(t) => {
            let t = t.trim_start_matches('#');
            let (seq, n) = match t.split_once('.') {
                Some((s, n)) => (s, Some(n)),
                None => (t, None),
            };
            let Ok(seq) = seq.parse::<i64>() else {
                return Ok(usage());
            };
            match n.map(str::parse::<usize>) {
                None => (seq, None),
                Some(Ok(n)) => (seq, Some(n)),
                Some(Err(_)) => return Ok(usage()),
            }
        }
    };
    let Some(x) = xray::turn(world, seq).map_err(failed)? else {
        return Ok(ContextReport::Text(format!(
            "Event #{seq} isn't a turn's input; :context alone lists the latest turn's calls."
        )));
    };
    let Some(n) = n else {
        return Ok(ContextReport::Text(xray::context_list(&x)));
    };
    match xray::turn_call(world, seq, n).map_err(failed)? {
        Some(call) => Ok(ContextReport::Call {
            seq,
            n,
            call: Box::new(call),
            raw,
        }),
        None => Ok(ContextReport::Text(match x.no_calls() {
            Some(why) => format!("Turn #{seq}: {why}."),
            None => format!(
                "Turn #{seq} has no call {n}; it made {} (:context #{seq} lists them).",
                x.calls.len()
            ),
        })),
    }
}

/// A call's heading line: `Turn #12 call 2 · director · sonnet`.
pub fn call_title(seq: i64, n: usize, call: &StoredCall) -> String {
    format!(
        "Turn #{seq} call {n} · {} · {}",
        call.call.role,
        xray::short_model(&call.call.model)
    )
}

/// Plain mode's `:context #seq.N`: the call's whole context tree as text,
/// or its JSON with `raw`.
pub fn call_text(
    world: &World,
    seq: i64,
    n: usize,
    call: &StoredCall,
    raw: bool,
) -> Result<String, String> {
    if raw {
        return Ok(xray::tree::raw_json(call));
    }
    let (tree, banner) =
        xray::call_tree(world, call).map_err(|e| format!("[:context unavailable: {e}]"))?;
    let mut out = call_title(seq, n, call);
    if let Some(b) = banner {
        out.push_str(&format!("\n! {b}"));
    }
    out.push_str("\n\n");
    out.push_str(&tree.text());
    Ok(out)
}

/// Something the engine says after a turn.
#[derive(Debug, Clone, PartialEq)]
pub enum Notice {
    /// A bracketed builder's note (fast path, stopped early, failures,
    /// ticks), shown dim.
    Engine(String),
    /// A line meant for the player (quest complete or failed).
    Story(String),
}

/// What to say after a turn, in order.
pub fn turn_notices(outcome: &Result<TurnOutcome, GameError>) -> Vec<Notice> {
    let outcome = match outcome {
        Ok(o) => o,
        Err(e) => return vec![Notice::Engine(format!("[turn failed: {e}]"))],
    };
    let mut notices = Vec::new();
    if let Some(reason) = &outcome.stop_reason {
        notices.push(Notice::Engine(format!("[stopped early: {reason:?}]")));
    }
    if let Some(rule) = outcome.fast_path {
        notices.push(Notice::Engine(format!("[fast path: rule #{rule}]")));
    }
    for j in &outcome.judgments {
        match j.verdict {
            Verdict::Complete => {
                notices.push(Notice::Story(format!("[Quest complete: {}]", j.title)))
            }
            Verdict::Failed => notices.push(Notice::Story(format!("[Quest failed: {}]", j.title))),
            Verdict::Ongoing => {}
        }
    }
    notices
}

/// The terse builder's line for a world tick.
pub fn tick_notice(outcome: &Result<TickOutcome, GameError>) -> String {
    match outcome {
        Ok(t) => format!("[world tick: {} change(s)]", t.changes.len()),
        Err(e) => format!("[world tick failed: {e}]"),
    }
}

/// A tick's full report for the `:tick` view: the summary and each change.
pub fn tick_report(outcome: &Result<TickOutcome, GameError>) -> String {
    let Ok(t) = outcome else {
        return tick_notice(outcome);
    };
    let mut out = tick_notice(outcome);
    if !t.summary.trim().is_empty() {
        out.push_str(&format!("\n\n{}", t.summary.trim()));
    }
    for change in &t.changes {
        out.push_str(&format!("\n- {}", change.note));
    }
    out
}

/// One line of the story so far, read back from the log.
#[derive(Debug, Clone, PartialEq)]
pub enum Recent {
    /// The player's input.
    Input(String),
    /// Narration.
    Narration(String),
}

/// The last `limit` inputs and narrations, oldest first (the seed prompt
/// left out), for a story pane that opens on a resumed save.
pub fn recent_story(world: &World, limit: usize) -> Result<Vec<Recent>, StoreError> {
    let last = world.last_seq()?;
    // Turns log a handful of events each; look back far enough for `limit`.
    let from = last.saturating_sub(limit as i64 * 40).max(0);
    let mut lines: Vec<Recent> = world
        .events_since(from)?
        .into_iter()
        .filter_map(|e| {
            let text = e.payload["text"].as_str()?.to_string();
            match e.kind {
                EventKind::Input if e.payload["seed"] != serde_json::json!(true) => {
                    Some(Recent::Input(text))
                }
                EventKind::Narration if !text.trim().is_empty() => Some(Recent::Narration(text)),
                _ => None,
            }
        })
        .collect();
    let skip = lines.len().saturating_sub(limit);
    lines.drain(..skip);
    Ok(lines)
}

/// The status bar's facts about the game, read from the store.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Status {
    /// "Day 2 · evening 18:40", once the clock exists.
    pub when: Option<String>,
    /// The player's room.
    pub room: Option<String>,
    /// What the calls made since the session started cost.
    pub cost_usd: f64,
}

/// Reads the status from `world`; `since_call` is the last call id before
/// the session started.
pub fn status(world: &World, since_call: i64) -> Result<Status, StoreError> {
    let when = if crate::game::bible_id(world)?.is_some() {
        let now = crate::time::now(world)?;
        Some(format!(
            "Day {} · {} {:02}:{:02}",
            now.day(),
            now.part(),
            now.hour(),
            now.minute()
        ))
    } else {
        None
    };
    let room = match crate::game::scope(world)? {
        Some(scope) => world.entity(scope.room)?.map(|r| r.name),
        None => None,
    };
    Ok(Status {
        when,
        room,
        cost_usd: world.cost_since(since_call)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::tests::begun;
    use crate::llm::{Response, ScriptedClient};

    #[test]
    fn completion_lists_matching_commands() {
        assert_eq!(complete(":qu"), [":quests", ":quit"]);
        assert_eq!(complete(":rul"), [":rules"]);
        assert!(complete(":zz").is_empty());
        let mut sorted = COMMANDS.to_vec();
        sorted.sort();
        assert_eq!(sorted, COMMANDS);
    }

    #[test]
    fn reports_cover_the_inspectors_and_nothing_else() {
        let (game, _) = begun();
        let world = game.world();
        for cmd in [
            ":rules", ":quests", ":npcs", ":stats", ":time", ":log", ":i",
        ] {
            assert!(matches!(report(world, cmd, ""), Some(Ok(_))), "{cmd}");
        }
        assert_eq!(
            report(world, ":fork", ""),
            Some(Ok("Usage: :fork <event> <path>".into()))
        );
        assert_eq!(report(world, ":tick", ""), None);
        assert_eq!(report(world, ":bogus", ""), None);
    }

    #[test]
    fn recent_story_reads_inputs_and_narration_but_not_the_seed() {
        let (mut game, _) = begun();
        let client = ScriptedClient::new([Response::canned_text("Water drips.")]);
        game.turn(&client, "listen", &mut |_| {}).unwrap();
        let story = recent_story(game.world(), 10).unwrap();
        assert_eq!(
            story,
            [
                Recent::Narration("You wake in a flooded chapel.".into()),
                Recent::Input("listen".into()),
                Recent::Narration("Water drips.".into()),
            ]
        );
        assert_eq!(recent_story(game.world(), 1).unwrap().len(), 1);
    }

    #[test]
    fn status_names_the_room_time_and_session_cost() {
        let (game, _) = begun();
        let s = status(game.world(), game.world().last_call_id().unwrap()).unwrap();
        assert_eq!(s.room.as_deref(), Some("Drowned Chapel"));
        assert!(s.when.unwrap().starts_with("Day 1"));
        assert_eq!(s.cost_usd, 0.0);
    }

    #[test]
    fn notices_follow_the_turn() {
        let failed: Result<TurnOutcome, GameError> = Err(GameError::NotStarted);
        assert!(
            matches!(&turn_notices(&failed)[..], [Notice::Engine(t)] if t.starts_with("[turn failed"))
        );
    }

    #[test]
    fn context_lists_a_turns_calls_and_shows_one() {
        let (game, seq) = crate::xray::tests::played();
        let world = game.world();
        let ContextReport::Text(list) = context(world, "").unwrap() else {
            panic!("expected the list");
        };
        assert!(list.contains(&format!("#{seq}.1 director")), "{list}");
        assert_eq!(
            context(world, &format!("#{seq}")).unwrap(),
            ContextReport::Text(list)
        );
        let ContextReport::Call { call, raw, n, .. } =
            context(world, &format!("#{seq}.2")).unwrap()
        else {
            panic!("expected a call");
        };
        assert_eq!((n, raw), (2, false));
        let text = call_text(world, seq, n, &call, false).unwrap();
        assert!(
            text.starts_with(&format!("Turn #{seq} call 2 · director · sonnet\n\n")),
            "{text}"
        );
        assert!(text.contains("Player input"));
        let ContextReport::Call { call, raw, .. } =
            context(world, &format!("--raw {seq}.2")).unwrap()
        else {
            panic!("expected a call");
        };
        assert!(raw);
        let json: serde_json::Value =
            serde_json::from_str(&call_text(world, seq, 2, &call, true).unwrap()).unwrap();
        assert_eq!(json["role"], "director");
    }

    #[test]
    fn context_explains_what_it_cannot_show() {
        let (game, seq) = crate::xray::tests::played();
        let world = game.world();
        let text = |arg: &str| match context(world, arg).unwrap() {
            ContextReport::Text(t) => t,
            other => panic!("expected text, got {other:?}"),
        };
        assert!(text("#1").contains("isn't a turn's input"));
        assert!(text(&format!("#{seq}.9")).contains("has no call 9; it made 5"));
        assert!(text("#x").starts_with("Usage"));
        assert!(text("#1 #2").starts_with("Usage"));
        let (fresh, _) = begun();
        assert_eq!(
            context(fresh.world(), "").unwrap(),
            ContextReport::Text("No turns played yet.".into())
        );
    }

    #[test]
    fn context_reports_no_records_for_turns_before_recording() {
        let (mut game, client) = begun();
        game.set_recording(false);
        client.push(Response::canned_text("Water drips."));
        game.turn(&client, "listen", &mut |_| {}).unwrap();
        let seq = crate::xray::latest_turn(game.world()).unwrap().unwrap();
        game.world().set_recorded_from(seq + 1);
        let ContextReport::Text(list) = context(game.world(), "").unwrap() else {
            panic!("expected text");
        };
        assert!(list.contains(crate::xray::PRE_RECORDING), "{list}");
        let ContextReport::Text(one) = context(game.world(), &format!("#{seq}.1")).unwrap() else {
            panic!("expected text");
        };
        assert_eq!(one, format!("Turn #{seq}: {}.", crate::xray::PRE_RECORDING));
    }

    #[test]
    fn help_lists_the_workbench_commands() {
        assert!(HELP.contains("Workbench"));
        assert!(HELP.contains(":xray [on|off]") && HELP.contains(":context"));
        assert_eq!(complete(":x"), [":xray"]);
        assert_eq!(complete(":con"), [":context"]);
    }

    #[test]
    fn the_plain_footer_follows_a_new_turn_only_when_xray_is_on() {
        let (game, seq) = crate::xray::tests::played();
        let world = game.world();
        let footer = xray_footer(world, None, true).unwrap();
        assert!(
            footer.starts_with("[director · sonnet director 5 calls"),
            "{footer}"
        );
        assert!(footer.contains("[refused: create \"gold\""));
        assert_eq!(xray_footer(world, None, false), None);
        // No new turn (the command failed before logging input).
        assert_eq!(xray_footer(world, Some(seq), true), None);
        assert!(xray_footer(world, Some(seq - 1), true).is_some());
    }

    #[test]
    fn xray_switches_on_and_off() {
        assert_eq!(xray_switch("", false), Ok(true));
        assert_eq!(xray_switch("", true), Ok(false));
        assert_eq!(xray_switch("on", true), Ok(true));
        assert_eq!(xray_switch("off", true), Ok(false));
        assert!(xray_switch("maybe", true).is_err());
        assert!(xray_from_env(Some("1")));
        assert!(!xray_from_env(Some("0")));
        assert!(!xray_from_env(None));
    }
}
