//! Terminal REPL: `loam [save.db]` (default `loam.db`).
//!
//! A new save asks for a seed prompt (ended by a blank line), builds the
//! world bible and opens the story; an existing save resumes where it was.

use std::io::{self, BufRead, Write};
use std::process::ExitCode;

use loam::game::Game;
use loam::llm::AnthropicClient;
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
            _ => {}
        }
        println!();
        match game.turn(&client, input, &mut stream) {
            Ok(outcome) => {
                if let Some(reason) = outcome.stop_reason {
                    eprintln!("\n[director stopped early: {reason:?}]");
                }
                println!();
            }
            // A failed turn is reported; the save stays usable.
            Err(e) => eprintln!("\n[turn failed: {e}]"),
        }
    }
    Ok(())
}
