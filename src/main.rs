//! Terminal REPL: `loam [save.db]` (default `loam.db`).
//!
//! A new save asks for a seed prompt (ended by a blank line), builds the
//! world bible and opens the story; an existing save resumes where it was.
//! A world tick runs between turns when due. `:quests` lists quests;
//! `:tick` forces a world tick; `:quit` exits.

use std::io::{self, BufRead, Write};
use std::process::ExitCode;

use loam::game::{Game, GameError, TickOutcome};
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
        match input {
            "" => continue,
            ":quit" | ":q" => break,
            ":quests" => {
                print_quests(&game);
                continue;
            }
            ":tick" => {
                report_tick(game.tick(&client));
                continue;
            }
            _ => {}
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

fn print_quests(game: &Game) {
    let quests = match game.quests() {
        Ok(q) => q,
        Err(e) => {
            eprintln!("[quests unavailable: {e}]");
            return;
        }
    };
    if quests.is_empty() {
        println!("No quests yet.");
    }
    for q in quests {
        println!("#{} {} [{}]", q.id, q.title, q.status.as_str());
        println!("  {}", q.goal.as_deref().unwrap_or(&q.hook));
        for d in &q.developments {
            println!("  - {d}");
        }
        if let Some(r) = &q.resolution {
            println!("  => {r}");
        }
    }
}

/// A terse builder's line on stderr; the player sees nothing of the tick.
fn report_tick(result: Result<TickOutcome, GameError>) {
    match result {
        Ok(t) => eprintln!("[world tick: {} change(s)]", t.changes.len()),
        Err(e) => eprintln!("[world tick failed: {e}]"),
    }
}
