//! What both front ends (the TUI and the plain REPL) share about `:`
//! commands and engine notices.
//!
//! Inspector commands read only the store, through whatever [`World`] they
//! are given (the front ends pass their read-only view), never a model.

use crate::explain::Topic;
use crate::game::{GameError, TickOutcome, TurnOutcome};
use crate::inspect;
use crate::quest::Verdict;
use crate::world::{EventKind, StoreError, StoredCall, World};
use crate::xray;

/// Every `:` command, for Tab completion and help.
pub const COMMANDS: &[&str] = &[
    ":adopt",
    ":brain",
    ":branches",
    ":compendium",
    ":context",
    ":explain",
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
    ":reroll",
    ":rerolls",
    ":rules",
    ":stats",
    ":surgery",
    ":switch",
    ":tick",
    ":time",
    ":why",
    ":xray",
];

/// The command list `:help` prints.
pub const HELP: &str = "Commands: :compendium [places | <name> | search <text>] (what \
you have learned), :rules, :glossary, :why <#id|name>, :relations <#id|name>, \
:quests, :npcs, :inventory (:i), :stats, :time, :pressures, :log [count|all] [kind…] | \
:log #<event>, :tick, :quit

Workbench (see and steer the models): :xray [on|off] shows each turn's calls, cache use, \
cost, refusals, rules and rolls; :context [#turn] lists a turn's calls, :context #turn.N \
[--raw] shows one call's full context (--raw: its JSON); :brain [<role>=<model>] \
[effort=<level>] | :brain reset shows or changes each role's model and effort; :branches \
shows this game's saves as a tree, :switch <n|name> plays another, :fork <event> [path.db] \
[label] makes a new branch; :reroll [#turn] [n] [director=<model>] [effort=<level>] \
[dice=fresh] reruns a turn n times (default 3, max 8) without touching the save and compares \
the variants, :reroll show <n> reads one, :adopt <n> plays it as a new branch; :rerolls lists \
past rerolls, :context r<id>.<n>[.<k>] shows a variant's calls; :surgery [#turn] [system] [n] \
[director=<model>] [effort=<level>] [dice=fresh] opens what the director was told for a turn \
(its turn message, or with system its system prompt) in $EDITOR, then plays n edited variants \
(default 2, marked ✂) beside an unedited control, always on the director path; an unchanged \
edit cancels; :explain [topic] explains the workbench's ideas (a note also explains each the \
first time it turns up).";

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
        ":compendium" => crate::knowledge::compendium(world, arg),
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
        ":branches" => branches(world),
        ":rerolls" => crate::reroll::list(world),
        ":fork" => return Some(fork(world, arg)),
        ":explain" => return Some(Ok(crate::explain::explain(arg))),
        _ => return None,
    };
    Some(result.map_err(|e| format!("[{cmd} unavailable: {e}]")))
}

/// The topics a workbench command's view involves, for first-time notes.
pub fn command_topics(cmd: &str) -> &'static [Topic] {
    match cmd {
        ":context" => &[Topic::Context],
        ":brain" => &[Topic::Brain],
        ":branches" | ":switch" | ":fork" | ":adopt" => &[Topic::Branch],
        _ => &[],
    }
}

/// A first-time note as plain mode prints it (dim).
pub fn note_line(topic: Topic) -> String {
    format!("[note · {}]", topic.line())
}

/// `:branches`: the tree of saves beside this one that share its lineage.
pub fn branches(world: &World) -> Result<String, StoreError> {
    let Some(path) = world.path() else {
        return Ok("This game isn't saved in a file, so it has no branches.".into());
    };
    let tree = crate::branches::tree(&crate::branches::scan(&path)?);
    Ok(format!(
        "{tree}\n\n* is the save being played. :switch <n|name> plays another; \
         :fork <event> [path.db] [label] makes a new branch."
    ))
}

/// `:fork <event> [path.db] [label…]`: branch the session at an event into
/// a new save. Without a path (a word ending in `.db`) the branch is named
/// `<stem>.b<N>.db` beside this save; any other words are its label. `Ok`
/// is what to tell the player (including usage mistakes); `Err` is an
/// engine notice.
pub fn fork(world: &World, arg: &str) -> Result<String, String> {
    const USAGE: &str = "Usage: :fork <event> [path.db] [label]";
    let mut parts = arg.split_whitespace().peekable();
    let Some(Ok(at)) = parts
        .next()
        .map(|n| n.trim_start_matches('#').parse::<i64>())
    else {
        return Ok(USAGE.into());
    };
    let given = parts.next_if(|p| p.ends_with(".db"));
    let path = match given {
        Some(p) => std::path::PathBuf::from(p),
        None => match world.path() {
            Some(current) => crate::branches::next_path(&current),
            None => {
                return Ok(format!(
                    "{USAGE} (this game has no save file to name it after)"
                ));
            }
        },
    };
    let label = parts.collect::<Vec<_>>().join(" ");
    // Checked first so a bad event number doesn't leave an empty save behind.
    match world.last_seq() {
        Ok(last) if (0..=last).contains(&at) => {}
        Ok(last) => return Ok(format!("No event #{at}: the log ends at #{last}.")),
        Err(e) => return Err(format!("[fork failed: {e}]")),
    }
    if path.exists() {
        return Ok(format!(
            "{} already exists; pick another path.",
            path.display()
        ));
    }
    let name = path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    // An auto-named branch sits beside the save: its name says enough.
    let shown = given.map_or_else(|| name.clone(), str::to_string);
    match world.fork(at, &path) {
        Ok(branch) => {
            if let Err(e) = branch.set_label(&label) {
                return Err(format!("[fork made {shown} but couldn't label it: {e}]"));
            }
            Ok(format!(
                "Forked at event #{at} into {shown}; :switch {name} plays it."
            ))
        }
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
    new_turn_xray(world, before, on).map(|x| xray::footer(&x))
}

/// The X-ray of the latest turn, if X-ray is `on` and a turn was played
/// after `before`.
pub fn new_turn_xray(world: &World, before: Option<i64>, on: bool) -> Option<xray::TurnXray> {
    if !on {
        return None;
    }
    let seq = xray::latest_turn(world).ok().flatten()?;
    if before.is_some_and(|b| b >= seq) {
        return None;
    }
    xray::turn(world, seq).ok().flatten()
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
    /// Call `k` of variant `variant` of reroll `reroll`.
    VariantCall {
        reroll: i64,
        variant: usize,
        k: usize,
        call: Box<StoredCall>,
        raw: bool,
    },
}

/// `:context [#seq | #seq.N | r<id>.<n>[.<k>]] [--raw]`: the latest
/// turn's (or turn `seq`'s, or a reroll variant's) calls, or one call.
/// `Err` is an engine notice.
pub fn context(world: &World, arg: &str) -> Result<ContextReport, String> {
    let failed = |e: StoreError| format!("[:context unavailable: {e}]");
    let usage = || {
        ContextReport::Text("Usage: :context [#turn | #turn.N | r<id>.<n>[.<k>]] [--raw]".into())
    };
    let mut raw = false;
    let mut target = None;
    for word in arg.split_whitespace() {
        match word {
            "--raw" => raw = true,
            _ if target.is_none() => target = Some(word),
            _ => return Ok(usage()),
        }
    }
    if let Some(variant) = target.and_then(|t| t.strip_prefix('r')) {
        let parts: Option<Vec<usize>> = variant.split('.').map(|p| p.parse().ok()).collect();
        return match parts.as_deref() {
            Some(&[id, n]) => variant_context(world, id as i64, n, None, raw).map_err(failed),
            Some(&[id, n, k]) => variant_context(world, id as i64, n, Some(k), raw).map_err(failed),
            _ => Ok(usage()),
        };
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

/// `:context r<id>.<n>`: variant `n` of reroll `id`'s calls, or call `k`.
fn variant_context(
    world: &World,
    id: i64,
    n: usize,
    k: Option<usize>,
    raw: bool,
) -> Result<ContextReport, StoreError> {
    let calls = world.variant_calls(id, n as i64)?;
    if calls.is_empty() {
        return Ok(ContextReport::Text(format!(
            "Reroll r{id} has no recorded calls for variant {n}; :rerolls lists rerolls."
        )));
    }
    let Some(k) = k else {
        let mut out = format!("Reroll r{id} variant {n}:");
        for (i, c) in calls.iter().enumerate() {
            let line = xray::CallLine::new(i + 1, c);
            out.push_str(&format!("\n  {}  {}", line.head(), line.usage()));
        }
        out.push_str(&format!("\n\n:context r{id}.{n}.<k> shows one call."));
        return Ok(ContextReport::Text(out));
    };
    Ok(
        match k.checked_sub(1).and_then(|i| calls.into_iter().nth(i)) {
            Some(call) => ContextReport::VariantCall {
                reroll: id,
                variant: n,
                k,
                call: Box::new(call),
                raw,
            },
            None => ContextReport::Text(format!("Reroll r{id} variant {n} has no call {k}.")),
        },
    )
}

/// A variant call's heading line: `Reroll r3 variant 2 call 1 · director · haiku`.
pub fn variant_call_title(reroll: i64, n: usize, k: usize, call: &StoredCall) -> String {
    format!(
        "Reroll r{reroll} variant {n} call {k} · {} · {}",
        call.call.role,
        xray::short_model(&call.call.model)
    )
}

/// A call's heading line: `Turn #12 call 2 · director · sonnet`.
pub fn call_title(seq: i64, n: usize, call: &StoredCall) -> String {
    format!(
        "Turn #{seq} call {n} · {} · {}",
        call.call.role,
        xray::short_model(&call.call.model)
    )
}

/// Plain mode's `:context #seq.N` (under `title`): the call's whole context tree as text,
/// or its JSON with `raw`.
pub fn call_text(
    world: &World,
    title: &str,
    call: &StoredCall,
    raw: bool,
) -> Result<String, String> {
    if raw {
        return Ok(xray::tree::raw_json(call));
    }
    let (tree, banner) =
        xray::call_tree(world, call).map_err(|e| format!("[:context unavailable: {e}]"))?;
    let mut out = title.to_owned();
    if let Some(b) = banner {
        out.push_str(&format!("\n! {b}"));
    }
    out.push_str("\n\n");
    out.push_str(&tree.text());
    Ok(out)
}

/// The save a `:switch <n|name>` argument names. `Err` is what to tell the
/// player: a usage line, no such branch, or it's already being played.
pub fn switch_target(world: &World, arg: &str) -> Result<std::path::PathBuf, String> {
    if arg.trim().is_empty() {
        return Err("Usage: :switch <n|name>; :branches lists them.".into());
    }
    let Some(current) = world.path() else {
        return Err("This game isn't saved in a file, so it has no branches.".into());
    };
    let branches =
        crate::branches::scan(&current).map_err(|e| format!("[:switch unavailable: {e}]"))?;
    match crate::branches::resolve(&branches, arg) {
        None => Err(format!("No branch {arg}; :branches lists them.")),
        Some(b) if b.current => Err(format!("You're already playing {}.", b.name())),
        Some(b) => Ok(b.path.clone()),
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
            Some(Ok("Usage: :fork <event> [path.db] [label]".into()))
        );
        assert_eq!(report(world, ":tick", ""), None);
        assert_eq!(report(world, ":bogus", ""), None);
    }

    #[test]
    fn fork_names_labels_and_refuses_clashes() {
        use crate::branches::tests::{saved_game, temp_dir};
        let dir = temp_dir("commands-fork");
        let main = saved_game(&dir, "mill.db");
        let world = World::open(&main).unwrap();
        assert_eq!(
            fork(&world, "#2 the long way round"),
            Ok("Forked at event #2 into mill.b1.db; :switch mill.b1.db plays it.".to_string())
        );
        let b1 = World::open(dir.join("mill.b1.db")).unwrap();
        assert_eq!(b1.label().unwrap().as_deref(), Some("the long way round"));
        assert_eq!(b1.forked_at().unwrap(), Some(2));
        drop(b1);
        let alt = dir.join("alt.db").display().to_string();
        assert!(
            fork(&world, &format!("1 {alt}"))
                .unwrap()
                .contains("alt.db")
        );
        assert_eq!(World::open(&alt).unwrap().label().unwrap(), None);
        assert!(
            fork(&world, &format!("1 {alt}"))
                .unwrap()
                .contains("already exists")
        );
        assert!(fork(&world, "999").unwrap().starts_with("No event #999"));

        // Plain mode prints the same tree.
        let text = branches(&world).unwrap();
        assert!(text.starts_with("* 1  mill.db"), "{text}");
        assert!(text.contains("├─ alt.db · from #1"), "{text}");
        assert!(
            text.contains("└─ mill.b1.db “the long way round” · from #2"),
            "{text}"
        );
        assert_eq!(
            switch_target(&world, "the long way round").unwrap(),
            std::fs::canonicalize(dir.join("mill.b1.db")).unwrap()
        );
        assert!(
            switch_target(&world, "1")
                .unwrap_err()
                .contains("already playing")
        );
        assert!(
            switch_target(&world, "9")
                .unwrap_err()
                .starts_with("No branch 9")
        );
        assert!(switch_target(&world, "").unwrap_err().starts_with("Usage"));
        drop(world);
        std::fs::remove_dir_all(&dir).unwrap();
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
        let text = call_text(world, &call_title(seq, n, &call), &call, false).unwrap();
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
            serde_json::from_str(&call_text(world, "", &call, true).unwrap()).unwrap();
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
    fn context_and_rerolls_reach_a_variants_calls() {
        use crate::llm::{LlmClient, Response, ScriptedClient};
        use crate::reroll;
        let (game, _) = crate::xray::tests::played();
        let world = game.world();
        assert!(reroll::list(world).unwrap().contains("No rerolls yet"));
        let scripts = [ScriptedClient::new([Response::canned_text("Again.")])];
        let r = reroll::run(
            world,
            &reroll::Spec::parse("1 director=haiku").unwrap(),
            game.brains(),
            &|n| &scripts[n - 1] as &(dyn LlmClient + Sync),
            &mut |_| {},
        )
        .unwrap();
        let id = r.head.id;
        let listed = report(world, ":rerolls", "").unwrap().unwrap();
        assert!(
            listed.contains(&format!("r{id} · turn #{}", r.head.turn_seq))
                && listed.contains("director haiku · same dice")
                && listed.contains("route: director 1/1"),
            "{listed}"
        );
        let ContextReport::Text(list) = context(world, &format!("r{id}.1")).unwrap() else {
            panic!("a list");
        };
        assert!(
            list.contains(&format!("Reroll r{id} variant 1:")) && list.contains("director"),
            "{list}"
        );
        let Ok(ContextReport::VariantCall {
            reroll,
            variant,
            k,
            call,
            raw,
        }) = context(world, &format!("r{id}.1.1 --raw"))
        else {
            panic!("one call");
        };
        assert_eq!((reroll, variant, k, raw), (id, 1, 1, true));
        assert_eq!(
            variant_call_title(reroll, variant, k, &call),
            format!("Reroll r{id} variant 1 call 1 · director · haiku")
        );
        assert!(
            matches!(context(world, &format!("r{id}.1.9")), Ok(ContextReport::Text(t)) if t.contains("no call 9"))
        );
        assert!(
            matches!(context(world, "r99.1"), Ok(ContextReport::Text(t)) if t.contains("no recorded calls"))
        );
        assert!(
            matches!(context(world, "rx"), Ok(ContextReport::Text(t)) if t.starts_with("Usage"))
        );
    }

    #[test]
    fn explain_prints_in_plain_mode_and_notes_follow_views_and_the_xray() {
        use crate::explain::{Notes, Topic, turn_topics};
        let (game, seq) = crate::xray::tests::played();
        let world = game.world();
        let Some(Ok(list)) = report(world, ":explain", "") else {
            panic!("a report");
        };
        assert!(
            list.starts_with("Topics (:explain <topic> for one):"),
            "{list}"
        );
        assert_eq!(
            report(world, ":explain", "cost"),
            Some(Ok(Topic::Cost.line()))
        );
        assert_eq!(complete(":ex"), [":explain"]);
        assert!(HELP.contains(":explain [topic]"));

        // What plain mode prints after a turn with X-ray on, once per save.
        let mut notes = Notes::load(world);
        let x = new_turn_xray(world, Some(seq - 1), true).unwrap();
        assert_eq!(new_turn_xray(world, Some(seq - 1), false), None);
        let fresh = notes.fresh(turn_topics(&x));
        assert_eq!(fresh[0], Topic::Director);
        assert_eq!(
            note_line(fresh[0]),
            format!("[note · director: {}]", Topic::Director.text())
        );
        assert!(notes.fresh(turn_topics(&x)).is_empty());
        assert_eq!(
            notes.fresh(command_topics(":brain").to_vec()),
            [Topic::Brain]
        );
        assert_eq!(command_topics(":adopt"), [Topic::Branch]);
        assert!(command_topics(":rules").is_empty());
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
