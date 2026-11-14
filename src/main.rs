//! Terminal REPL: `loam [save.db]` (default `loam.db`).
//!
//! A new save asks for a seed prompt (ended by a blank line), builds the
//! world bible and opens the story; an existing save resumes where it was.
//! A world tick runs between turns when due. Lines starting with `:` are
//! commands (`:help` lists them); the inspector commands read only the
//! store and its log, never the LLM.

use std::io::{self, BufRead, Write};
use std::process::ExitCode;

use loam::game::{Game, GameError, TickOutcome};
use loam::inspect;
use loam::llm::AnthropicClient;
use loam::quest::Verdict;
use loam::world::World;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).unwrap_or_else(|| "loam.db".into());
    let client = AnthropicClient::from_env()?;
    let mut game = Game::new(World::open(&path)?);
    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();
    let mut stream = |t: &str| {
        print!("{t}");
        let _ = io::stdout().flush();
    };

    if !game.has_bible()? {
        println!(
            "New world in {path}. Describe its plot, tone and setting; end with a blank line."
        );
        let mut seed = String::new();
        for line in lines.by_ref() {
            let line = line?;
            if line.trim().is_empty() && !seed.trim().is_empty() {
                break;
            }
            seed.push_str(&line);
            seed.push('\n');
        }
        if seed.trim().is_empty() {
            return Err("no seed prompt given".into());
        }
        println!("Building the world…");
        game.create_bible(&client, seed.trim())?;
    }
    if !game.has_begun()? {
        game.begin(&client, &mut stream)?;
        println!();
    } else {
        println!("Resuming {path}.\n\n{}", game.look()?);
    }

    loop {
        print!("\n> ");
        io::stdout().flush()?;
        let Some(line) = lines.next() else { break };
        let input = line?;
        let input = input.trim();
        if input.is_empty() {
            continue;
        }
        if input.starts_with(':') {
            let (cmd, arg) = input.split_once(char::is_whitespace).unwrap_or((input, ""));
            let arg = arg.trim();
            let world = game.world();
            match cmd {
                ":quit" | ":q" => break,
                ":tick" => report_tick(game.tick(&client)),
                ":rules" => show(cmd, inspect::rules(world)),
                ":glossary" => show(cmd, inspect::glossary(world)),
                ":why" => show(cmd, inspect::why(world, arg)),
                ":quests" => show(cmd, inspect::quests(world)),
                ":stats" => show(cmd, inspect::stats(world)),
                ":fork" => fork(world, arg),
                ":help" => println!("{HELP}"),
                _ => println!("Unknown command {cmd}. {HELP}"),
            }
            continue;
        }
        println!();
        match game.turn(&client, input, &mut stream) {
            Ok(outcome) => {
                if let Some(reason) = outcome.stop_reason {
                    eprintln!("\n[stopped early: {reason:?}]");
                }
                if let Some(rule) = outcome.fast_path {
                    eprintln!("\n[fast path: rule #{rule}]");
                }
                println!();
                for j in &outcome.judgments {
                    match j.verdict {
                        Verdict::Complete => println!("\n[Quest complete: {}]", j.title),
                        Verdict::Failed => println!("\n[Quest failed: {}]", j.title),
                        Verdict::Ongoing => {}
                    }
                }
                // The world moves on off-screen; a failed tick is retried
                // after the next turn.
                if let Some(result) = game.maybe_tick(&client).transpose() {
                    report_tick(result);
                }
            }
            // A failed turn is reported; the save stays usable.
            Err(e) => eprintln!("\n[turn failed: {e}]"),
        }
    }
    Ok(())
}

const HELP: &str = "Commands: :rules, :glossary, :why <#id|name>, :quests, :stats, \
:fork <event> <path>, :tick, :quit";

/// Print an inspector report; a failure leaves the save usable.
fn show(cmd: &str, report: Result<String, loam::world::StoreError>) {
    match report {
        Ok(text) => println!("{text}"),
        Err(e) => eprintln!("[{cmd} unavailable: {e}]"),
    }
}

/// `:fork <event> <path>`: branch the session at an event into a new save.
fn fork(world: &World, arg: &str) {
    let mut parts = arg.split_whitespace();
    let (Some(Ok(at)), Some(path), None) = (
        parts
            .next()
            .map(|n| n.trim_start_matches('#').parse::<i64>()),
        parts.next(),
        parts.next(),
    ) else {
        println!("Usage: :fork <event> <path>");
        return;
    };
    // Checked first so a bad event number doesn't leave an empty save behind.
    match world.last_seq() {
        Ok(last) if (0..=last).contains(&at) => {}
        Ok(last) => {
            println!("No event #{at}: the log ends at #{last}.");
            return;
        }
        Err(e) => {
            eprintln!("[fork failed: {e}]");
            return;
        }
    }
    match world.fork(at, path) {
        Ok(_) => println!("Forked at event #{at} into {path}; run `loam {path}` to play it."),
        Err(e) => eprintln!("[fork failed: {e}]"),
    }
}

/// A terse builder's line on stderr; the player sees nothing of the tick.
fn report_tick(result: Result<TickOutcome, GameError>) {
    match result {
        Ok(t) => eprintln!("[world tick: {} change(s)]", t.changes.len()),
        Err(e) => eprintln!("[world tick failed: {e}]"),
    }
}
