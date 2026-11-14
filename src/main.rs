//! Terminal REPL: `loam [--seed <name|file>] [save.db]`.
//!
//! A new save takes its seed prompt from `--seed` (a file, or the name of one
//! in `seeds/`, such as `drowned-mill`) or else asks for one (ended by a blank
//! line), builds the world bible and opens the story; an existing save resumes
//! where it was. The save defaults to `<seed name>.db` with `--seed`, else
//! `loam.db`. `--list-seeds` lists the bundled seeds.
//! A world tick runs between turns when due. Lines starting with `:` are
//! commands (`:help` lists them); the inspector commands read only the
//! store and its log, never the LLM.
//!
//! Variables such as `ANTHROPIC_API_KEY` can live in a `.env` file, read from
//! the current directory and then the repo root; real environment variables
//! win.

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
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
    load_env_files();
    let args = match parse_args(std::env::args().skip(1))? {
        Command::Help => {
            println!("{USAGE}");
            return Ok(());
        }
        Command::ListSeeds => {
            let names = list_seeds(&seed_dirs());
            if names.is_empty() {
                println!("No seeds found in {}.", seeds_dir_names());
            }
            for name in names {
                println!("{name}");
            }
            return Ok(());
        }
        Command::Play(args) => args,
    };
    // Read the seed before anything else so a typo fails fast.
    let seed_file = match &args.seed {
        Some(arg) => {
            let file = resolve_seed(arg, &seed_dirs())?;
            Some((std::fs::read_to_string(&file)?, file))
        }
        None => None,
    };
    let path = args.save;
    let client = AnthropicClient::from_env()?;
    let mut game = Game::new(World::open(&path)?);
    if seed_file.is_some() && game.has_bible()? {
        eprintln!("[{path} already has a world; --seed ignored]");
    }
    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();
    let mut stream = |t: &str| {
        print!("{t}");
        let _ = io::stdout().flush();
    };

    if !game.has_bible()? {
        let mut seed = String::new();
        if let Some((text, file)) = &seed_file {
            println!("New world in {path}, seeded from {}.", file.display());
            seed.push_str(text);
        } else {
            println!(
                "New world in {path}. Describe its plot, tone and setting; end with a blank line."
            );
            for line in lines.by_ref() {
                let line = line?;
                if line.trim().is_empty() && !seed.trim().is_empty() {
                    break;
                }
                seed.push_str(&line);
                seed.push('\n');
            }
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
                ":npcs" => show(cmd, inspect::npcs(world)),
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

const USAGE: &str = "Usage: loam [--seed <name|file>] [save.db]
       loam --list-seeds

  --seed <name|file>  Seed a new save from a file, or from seeds/<name>.txt.
                      The save defaults to <name>.db (otherwise loam.db).
                      Ignored when the save already has a world.
  --list-seeds        List the bundled seed prompts.";

enum Command {
    Help,
    ListSeeds,
    Play(PlayArgs),
}

#[derive(Debug, PartialEq)]
struct PlayArgs {
    save: String,
    seed: Option<String>,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Command, String> {
    let mut seed = None;
    let mut save = None;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(Command::Help),
            "--list-seeds" => return Ok(Command::ListSeeds),
            "--seed" => {
                seed = Some(args.next().ok_or("--seed needs a name or file")?);
            }
            _ if arg.starts_with("--seed=") => seed = Some(arg["--seed=".len()..].to_string()),
            _ if arg.starts_with('-') => return Err(format!("unknown option {arg}\n\n{USAGE}")),
            _ if save.is_none() => save = Some(arg),
            _ => return Err(format!("unexpected argument {arg}\n\n{USAGE}")),
        }
    }
    let save = save.unwrap_or_else(|| match &seed {
        Some(s) => format!("{}.db", seed_name(s)),
        None => "loam.db".into(),
    });
    Ok(Command::Play(PlayArgs { save, seed }))
}

/// `seeds/drowned-mill.txt` and `drowned-mill` both name `drowned-mill`.
fn seed_name(arg: &str) -> String {
    Path::new(arg)
        .file_stem()
        .map_or_else(|| arg.to_string(), |s| s.to_string_lossy().into_owned())
}

/// Where named seeds are looked up: `./seeds`, then the crate's own `seeds/`
/// (so `cargo run` and the built binary both find them).
fn seed_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![PathBuf::from("seeds")];
    let bundled = Path::new(env!("CARGO_MANIFEST_DIR")).join("seeds");
    if !dirs.contains(&bundled) {
        dirs.push(bundled);
    }
    dirs
}

fn env_files() -> Vec<PathBuf> {
    let mut files = vec![PathBuf::from(".env")];
    let bundled = Path::new(env!("CARGO_MANIFEST_DIR")).join(".env");
    if !files.contains(&bundled) {
        files.push(bundled);
    }
    files
}

/// Sets each variable from the `.env` files that isn't already set, so the
/// real environment and earlier files win.
fn load_env_files() {
    for file in env_files() {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        for (key, value) in parse_env(&text) {
            if std::env::var_os(&key).is_none() {
                // SAFETY: runs at the start of `run`, before any other threads exist.
                unsafe { std::env::set_var(key, value) };
            }
        }
    }
}

/// Parses `KEY=value` lines, skipping blanks and `#` comments. An optional
/// `export ` prefix and matching quotes around the value are dropped.
fn parse_env(text: &str) -> Vec<(String, String)> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let l = l.strip_prefix("export ").unwrap_or(l);
            let (key, value) = l.split_once('=')?;
            let key = key.trim();
            let value = value.trim();
            let value = [('"', '"'), ('\'', '\'')]
                .iter()
                .find_map(|&(a, b)| value.strip_prefix(a)?.strip_suffix(b))
                .unwrap_or(value);
            (!key.is_empty()).then(|| (key.to_string(), value.to_string()))
        })
        .collect()
}

fn seeds_dir_names() -> String {
    seed_dirs()
        .iter()
        .map(|d| d.display().to_string())
        .collect::<Vec<_>>()
        .join(" or ")
}

/// A path to an existing file wins; otherwise `<dir>/<arg>.txt` or
/// `<dir>/<arg>` in each seed directory, first match.
fn resolve_seed(arg: &str, dirs: &[PathBuf]) -> Result<PathBuf, String> {
    let direct = PathBuf::from(arg);
    if direct.is_file() {
        return Ok(direct);
    }
    for dir in dirs {
        for candidate in [dir.join(format!("{arg}.txt")), dir.join(arg)] {
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    let names = list_seeds(dirs);
    Err(if names.is_empty() {
        format!("no seed file or bundled seed named {arg}")
    } else {
        format!(
            "no seed file or bundled seed named {arg}; bundled seeds: {}",
            names.join(", ")
        )
    })
}

/// Names of the `.txt` seeds in the seed directories, sorted, deduplicated.
fn list_seeds(dirs: &[PathBuf]) -> Vec<String> {
    let mut names: Vec<String> = dirs
        .iter()
        .filter_map(|d| std::fs::read_dir(d).ok())
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "txt") && p.is_file())
        .map(|p| seed_name(&p.to_string_lossy()))
        .collect();
    names.sort();
    names.dedup();
    names
}

const HELP: &str = "Commands: :rules, :glossary, :why <#id|name>, :quests, :npcs, :stats, \
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

#[cfg(test)]
mod tests {
    use super::*;

    fn play(args: &[&str]) -> PlayArgs {
        match parse_args(args.iter().map(|s| s.to_string())) {
            Ok(Command::Play(p)) => p,
            Ok(_) => panic!("expected play args"),
            Err(e) => panic!("{e}"),
        }
    }

    #[test]
    fn save_defaults_to_loam_db_without_a_seed() {
        assert_eq!(
            play(&[]),
            PlayArgs {
                save: "loam.db".into(),
                seed: None
            }
        );
    }

    #[test]
    fn save_defaults_to_seed_name() {
        assert_eq!(play(&["--seed", "drowned-mill"]).save, "drowned-mill.db");
        assert_eq!(
            play(&["--seed=seeds/kettle-station.txt"]).save,
            "kettle-station.db"
        );
    }

    #[test]
    fn explicit_save_wins_in_either_order() {
        let a = play(&["--seed", "drowned-mill", "mine.db"]);
        let b = play(&["mine.db", "--seed", "drowned-mill"]);
        assert_eq!(a, b);
        assert_eq!(a.save, "mine.db");
        assert_eq!(a.seed.as_deref(), Some("drowned-mill"));
    }

    #[test]
    fn bad_arguments_are_errors() {
        assert!(parse_args(["--seed".to_string()]).is_err());
        assert!(parse_args(["--bogus".to_string()]).is_err());
        assert!(parse_args(["a.db".to_string(), "b.db".to_string()]).is_err());
    }

    #[test]
    fn bundled_seeds_resolve_by_name_and_list() {
        let dirs = vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("seeds")];
        let names = list_seeds(&dirs);
        assert_eq!(names, ["drowned-mill", "kettle-station", "ledger-of-ash"]);
        for name in &names {
            let file = resolve_seed(name, &dirs).unwrap();
            assert!(!std::fs::read_to_string(file).unwrap().trim().is_empty());
        }
        let err = resolve_seed("nope", &dirs).unwrap_err();
        assert!(err.contains("drowned-mill"), "{err}");
    }

    #[test]
    fn parse_env_reads_keys_and_strips_quotes() {
        let text = "# key\n\nA=1\nexport B = \"two words\"\nC='x=y'\nnot a pair\n=empty\n";
        let pairs = parse_env(text);
        let want = [("A", "1"), ("B", "two words"), ("C", "x=y")];
        assert_eq!(
            pairs,
            want.map(|(k, v)| (k.to_string(), v.to_string())).to_vec()
        );
    }
}
