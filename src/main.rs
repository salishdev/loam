//! Terminal REPL: `loam [--seed <name|file> | --prompt <text>] [save.db]`.
//!
//! A new save takes its seed prompt from `--seed` (a file, or the name of one
//! in `./seeds/` or compiled into the binary, such as `drowned-mill`), from
//! `--prompt`, or else offers a menu of the named seeds with an option to
//! write your own (ended by a blank line); it then builds the world bible and
//! opens the story; an existing save resumes where it was. The save defaults to
//! `<seed name>.db` with `--seed`, else `loam.db`. `--list-seeds` lists the
//! named seeds.
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
            for name in list_seeds(&seed_dirs()) {
                println!("{name}");
            }
            return Ok(());
        }
        Command::Play(args) => args,
    };
    // Read the seed before anything else so a typo fails fast.
    let seed_file = match (&args.seed, &args.prompt) {
        (Some(arg), _) => Some(resolve_seed(arg, &seed_dirs())?),
        (None, Some(text)) if text.trim().is_empty() => return Err("--prompt is empty".into()),
        (None, Some(text)) => Some(Seed {
            text: text.clone(),
            source: "your prompt".into(),
        }),
        (None, None) => None,
    };
    let path = args.save;
    let client = AnthropicClient::from_env()?;
    let mut game = Game::new(World::open(&path)?);
    if seed_file.is_some() && game.has_bible()? {
        eprintln!("[{path} already has a world; the seed is ignored]");
    }
    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();
    let mut stream = |t: &str| {
        print!("{t}");
        let _ = io::stdout().flush();
    };

    if !game.has_bible()? {
        let mut seed = String::new();
        if let Some(found) = &seed_file {
            println!("New world in {path}, seeded from {}.", found.source);
            seed.push_str(&found.text);
        } else if let Some(found) = choose_seed(&path, &mut lines)? {
            println!("Seeded from {}.", found.source);
            seed.push_str(&found.text);
        } else {
            println!("Describe the world's plot, tone and setting; end with a blank line.");
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
                ":relations" => show(cmd, inspect::relations(world, arg)),
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

const USAGE: &str = "Usage: loam [--seed <name|file> | --prompt <text>] [save.db]
       loam --list-seeds

  A new save without --seed or --prompt asks you to pick a named seed or
  write your own.

  --seed <name|file>  Seed a new save from a file, from seeds/<name>.txt, or
                      from the seed of that name built into loam.
                      The save defaults to <name>.db (otherwise loam.db).
                      Ignored when the save already has a world.
  --prompt <text>     Seed a new save from your own prompt. The save
                      defaults to loam.db.
  --list-seeds        List the named seed prompts (./seeds and built-in).";

enum Command {
    Help,
    ListSeeds,
    Play(PlayArgs),
}

#[derive(Debug, PartialEq)]
struct PlayArgs {
    save: String,
    seed: Option<String>,
    prompt: Option<String>,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Command, String> {
    let mut seed = None;
    let mut prompt = None;
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
            "--prompt" => {
                prompt = Some(args.next().ok_or("--prompt needs the prompt text")?);
            }
            _ if arg.starts_with("--prompt=") => {
                prompt = Some(arg["--prompt=".len()..].to_string());
            }
            _ if arg.starts_with('-') => return Err(format!("unknown option {arg}\n\n{USAGE}")),
            _ if save.is_none() => save = Some(arg),
            _ => return Err(format!("unexpected argument {arg}\n\n{USAGE}")),
        }
    }
    if seed.is_some() && prompt.is_some() {
        return Err(format!("use --seed or --prompt, not both\n\n{USAGE}"));
    }
    let save = save.unwrap_or_else(|| match &seed {
        Some(s) => format!("{}.db", seed_name(s)),
        None => "loam.db".into(),
    });
    Ok(Command::Play(PlayArgs { save, seed, prompt }))
}

/// `seeds/drowned-mill.txt` and `drowned-mill` both name `drowned-mill`.
fn seed_name(arg: &str) -> String {
    Path::new(arg)
        .file_stem()
        .map_or_else(|| arg.to_string(), |s| s.to_string_lossy().into_owned())
}

/// Where named seed files are looked up before the built-in seeds.
fn seed_dirs() -> Vec<PathBuf> {
    vec![PathBuf::from("seeds")]
}

/// The repo's `seeds/*.txt`, compiled in so a release binary has them wherever
/// it runs. A test checks this list matches the folder.
const BUILTIN_SEEDS: &[(&str, &str)] = &[
    ("drowned-mill", include_str!("../seeds/drowned-mill.txt")),
    (
        "kettle-station",
        include_str!("../seeds/kettle-station.txt"),
    ),
    ("ledger-of-ash", include_str!("../seeds/ledger-of-ash.txt")),
];

/// A seed prompt and where it came from, for the "seeded from" line.
#[derive(Debug)]
struct Seed {
    text: String,
    source: String,
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

/// A path to an existing file wins; then `<dir>/<arg>.txt` or `<dir>/<arg>`
/// in each seed directory, first match; then the built-in seed of that name.
fn resolve_seed(arg: &str, dirs: &[PathBuf]) -> Result<Seed, String> {
    let direct = PathBuf::from(arg);
    let candidates = dirs
        .iter()
        .flat_map(|dir| [dir.join(format!("{arg}.txt")), dir.join(arg)]);
    if let Some(file) = std::iter::once(direct)
        .chain(candidates)
        .find(|p| p.is_file())
    {
        let text = std::fs::read_to_string(&file)
            .map_err(|e| format!("can't read seed {}: {e}", file.display()))?;
        return Ok(Seed {
            text,
            source: file.display().to_string(),
        });
    }
    if let Some((name, text)) = BUILTIN_SEEDS.iter().find(|(name, _)| *name == arg) {
        return Ok(Seed {
            text: text.to_string(),
            source: format!("the built-in seed {name}"),
        });
    }
    Err(format!(
        "no seed file or named seed {arg}; named seeds: {}",
        list_seeds(dirs).join(", ")
    ))
}

/// Names of the `.txt` seeds in the seed directories and the built-in seeds,
/// sorted, deduplicated.
fn list_seeds(dirs: &[PathBuf]) -> Vec<String> {
    let mut names: Vec<String> = dirs
        .iter()
        .filter_map(|d| std::fs::read_dir(d).ok())
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "txt") && p.is_file())
        .map(|p| seed_name(&p.to_string_lossy()))
        .chain(BUILTIN_SEEDS.iter().map(|(name, _)| name.to_string()))
        .collect();
    names.sort();
    names.dedup();
    names
}

/// What the player picked from the new-world menu.
#[derive(Debug, PartialEq)]
enum SeedChoice {
    Named(String),
    Own,
}

/// Reads a menu answer: an item number, a seed name, or "own". `None` for
/// anything else, so the menu asks again.
fn parse_seed_choice(answer: &str, names: &[String]) -> Option<SeedChoice> {
    let answer = answer.trim();
    if let Ok(n) = answer.parse::<usize>() {
        return match n {
            0 => None,
            n if n <= names.len() => Some(SeedChoice::Named(names[n - 1].clone())),
            n if n == names.len() + 1 => Some(SeedChoice::Own),
            _ => None,
        };
    }
    if answer.eq_ignore_ascii_case("own") {
        return Some(SeedChoice::Own);
    }
    names
        .iter()
        .find(|n| n.eq_ignore_ascii_case(answer))
        .map(|n| SeedChoice::Named(n.clone()))
}

/// The seed's first sentence, cut to fit a menu line.
fn seed_teaser(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let first = flat.split_inclusive(". ").next().unwrap_or("").trim_end();
    let first = first.strip_suffix('.').unwrap_or(first);
    if first.chars().count() <= 60 {
        first.to_string()
    } else {
        format!("{}…", first.chars().take(59).collect::<String>().trim_end())
    }
}

/// The new-world menu: a named seed, or `None` to write your own. Ends the
/// game if input runs out before an answer.
fn choose_seed(
    path: &str,
    lines: &mut impl Iterator<Item = io::Result<String>>,
) -> Result<Option<Seed>, Box<dyn std::error::Error>> {
    let dirs = seed_dirs();
    let names = list_seeds(&dirs);
    println!("New world in {path}. Pick a seed:");
    for (i, name) in names.iter().enumerate() {
        let teaser =
            resolve_seed(name, &dirs).map_or_else(|_| String::new(), |s| seed_teaser(&s.text));
        println!("  {}. {name} — {teaser}", i + 1);
    }
    println!("  {}. Write your own", names.len() + 1);
    loop {
        print!("> ");
        io::stdout().flush()?;
        let Some(answer) = lines.next() else {
            return Err("no seed chosen".into());
        };
        match parse_seed_choice(&answer?, &names) {
            Some(SeedChoice::Named(name)) => return Ok(Some(resolve_seed(&name, &dirs)?)),
            Some(SeedChoice::Own) => return Ok(None),
            None => println!("Type a number from 1 to {}.", names.len() + 1),
        }
    }
}

const HELP: &str = "Commands: :rules, :glossary, :why <#id|name>, :relations <#id|name>, \
:quests, :npcs, :stats, :fork <event> <path>, :tick, :quit";

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
                seed: None,
                prompt: None,
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
    fn prompt_takes_inline_text_and_defaults_to_loam_db() {
        let a = play(&["--prompt", "a lighthouse at the end of the world"]);
        assert_eq!(
            a.prompt.as_deref(),
            Some("a lighthouse at the end of the world")
        );
        assert_eq!(a.save, "loam.db");
        let b = play(&["--prompt=fog", "fog.db"]);
        assert_eq!(
            (b.prompt.as_deref(), b.save.as_str()),
            (Some("fog"), "fog.db")
        );
    }

    #[test]
    fn seed_and_prompt_together_are_an_error() {
        let args = ["--seed", "drowned-mill", "--prompt", "x"].map(String::from);
        assert!(parse_args(args).is_err());
        assert!(parse_args(["--prompt".to_string()]).is_err());
    }

    #[test]
    fn seed_menu_takes_numbers_names_or_own() {
        let names: Vec<String> = ["drowned-mill", "kettle-station"]
            .map(String::from)
            .to_vec();
        let named = |n: &str| Some(SeedChoice::Named(n.to_string()));
        assert_eq!(parse_seed_choice("1", &names), named("drowned-mill"));
        assert_eq!(parse_seed_choice(" 2 ", &names), named("kettle-station"));
        assert_eq!(parse_seed_choice("3", &names), Some(SeedChoice::Own));
        assert_eq!(parse_seed_choice("own", &names), Some(SeedChoice::Own));
        assert_eq!(
            parse_seed_choice("Kettle-Station", &names),
            named("kettle-station")
        );
        for bad in ["0", "4", "", "mill"] {
            assert_eq!(parse_seed_choice(bad, &names), None, "{bad:?}");
        }
    }

    #[test]
    fn seed_teaser_is_the_first_sentence_cut_to_fit() {
        assert_eq!(seed_teaser("A cold moon.\nSix crew. More."), "A cold moon");
        let long = seed_teaser(&"word ".repeat(30));
        assert!(long.ends_with('…') && long.chars().count() <= 60, "{long}");
    }

    #[test]
    fn bad_arguments_are_errors() {
        assert!(parse_args(["--seed".to_string()]).is_err());
        assert!(parse_args(["--bogus".to_string()]).is_err());
        assert!(parse_args(["a.db".to_string(), "b.db".to_string()]).is_err());
    }

    #[test]
    fn builtin_seeds_resolve_without_any_seed_folder() {
        let names = list_seeds(&[]);
        assert_eq!(names, ["drowned-mill", "kettle-station", "ledger-of-ash"]);
        for name in &names {
            let seed = resolve_seed(name, &[]).unwrap();
            assert!(!seed.text.trim().is_empty());
            assert_eq!(seed.source, format!("the built-in seed {name}"));
        }
        let err = resolve_seed("nope", &[]).unwrap_err();
        assert!(err.contains("drowned-mill"), "{err}");
    }

    #[test]
    fn builtin_seeds_match_the_seeds_folder() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("seeds");
        let mut on_disk: Vec<(String, String)> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|x| x == "txt"))
            .map(|p| {
                let name = seed_name(&p.to_string_lossy());
                (name, std::fs::read_to_string(p).unwrap())
            })
            .collect();
        on_disk.sort();
        let builtin: Vec<(String, String)> = BUILTIN_SEEDS
            .iter()
            .map(|(n, t)| (n.to_string(), t.to_string()))
            .collect();
        assert_eq!(builtin, on_disk, "add new seeds/*.txt to BUILTIN_SEEDS");
    }

    #[test]
    fn a_seed_folder_file_wins_over_the_builtin_one() {
        let dir = std::env::temp_dir().join(format!("loam-seeds-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("drowned-mill.txt"), "my own mill").unwrap();
        std::fs::write(dir.join("extra.txt"), "extra").unwrap();
        let dirs = [dir.clone()];
        let seed = resolve_seed("drowned-mill", &dirs).unwrap();
        assert_eq!(seed.text, "my own mill");
        let names = list_seeds(&dirs);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            names,
            ["drowned-mill", "extra", "kettle-station", "ledger-of-ash"]
        );
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
