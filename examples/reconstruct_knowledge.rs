//! One-off evidence-only reconstruction. See docs/reconstruction.md.
#[path = "support/reconstruction_fixture.rs"]
mod fixture;

use loam::knowledge::reconstruct::{Options, Result, reconstruct};
use loam::llm::{AnthropicClient, LlmClient, ScriptedClient};
use std::path::Path;

const HELP: &str = "Usage:
  cargo run --example reconstruct_knowledge -- SOURCE.db NEW.db REPORT.txt [CHUNK_CHARS]
  cargo run --example reconstruct_knowledge -- --fixture NEW_SOURCE.db NEW.db REPORT.txt

Close SOURCE first. Only schema 6 is supported. All output paths must be new.
Live extraction needs ANTHROPIC_API_KEY in the environment (no automatic .env load).
--fixture creates an offline narrated save and uses ScriptedClient: no model/network.
Failures leave an INCOMPLETE report and possibly a partial output. Never use partial
outputs as input. See docs/reconstruction.md for evidence limits and cleanup.";

fn main() -> Result<()> {
    let mut args: Vec<_> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|s| s == "--help" || s == "-h") {
        println!("{HELP}");
        return Ok(());
    }
    let fixture = args.first().is_some_and(|s| s == "--fixture");
    if fixture {
        args.remove(0);
    }
    if !(args.len() == 3 || (!fixture && args.len() == 4)) {
        return Err(HELP.into());
    }
    let source = Path::new(&args[0]);
    let destination = Path::new(&args[1]);
    let report = Path::new(&args[2]);
    let (client, options): (Box<dyn LlmClient>, Options) = if fixture {
        (
            Box::new(ScriptedClient::new(fixture::create(source)?)),
            Options {
                chunk_chars: fixture::CHUNK_CHARS,
            },
        )
    } else {
        (
            Box::new(AnthropicClient::from_env()?),
            Options {
                chunk_chars: args
                    .get(3)
                    .map(|s| s.parse())
                    .transpose()?
                    .unwrap_or(Options::default().chunk_chars),
            },
        )
    };
    reconstruct(source, destination, report, client.as_ref(), options)?;
    let world = loam::world::World::open(destination)?;
    println!(
        "{}\n\nEvidence report: {}",
        loam::knowledge::compendium(&world, "player")?,
        report.display()
    );
    for e in loam::knowledge::entries(&world)? {
        println!(
            "\n{}",
            loam::knowledge::compendium(&world, &format!("player #{}", e.id))?
        );
    }
    println!("\n{}", loam::knowledge::compendium(&world, "world")?);
    Ok(())
}
