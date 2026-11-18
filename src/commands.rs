//! What both front ends (the TUI and the plain REPL) share about `:`
//! commands and engine notices.
//!
//! Inspector commands read only the store, through whatever [`World`] they
//! are given (the front ends pass their read-only view), never a model.

use crate::game::{GameError, TickOutcome, TurnOutcome};
use crate::inspect;
use crate::quest::Verdict;
use crate::world::{EventKind, StoreError, World};

/// Every `:` command, for Tab completion and help.
pub const COMMANDS: &[&str] = &[
    ":brain",
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
];

/// The command list `:help` prints.
pub const HELP: &str = "Commands: :rules, :glossary, :why <#id|name>, :relations <#id|name>, \
:quests, :npcs, :inventory (:i), :stats, :time, :pressures, :log [count|all] [kind…] | \
:log #<event>, :fork <event> <path>, :tick, :brain [<role>=<model>] [effort=<level>] | \
:brain reset, :quit";

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
}
