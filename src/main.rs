//! Loam's front ends: `loam [--plain] [--seed <name|file> | --prompt <text>] [save.db]`.
//!
//! On a terminal loam opens the full-screen TUI ([`loam::tui`]); `--plain`,
//! `LOAM_PLAIN=1` or a stdout that isn't a terminal gives the line REPL.
//! Both drive the same game worker ([`loam::worker`]) and read inspector
//! reports through a read-only connection to the save.
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
//! Output sits behind a left margin (`LOAM_MARGIN`, default 2) and the prompt
//! sits in it, so your lines start further left than the game's. On a
//! terminal the prompt is coloured, typed input is bold and the engine's
//! bracketed notices are dim; `NO_COLOR` turns colour off.
//!
//! Every model call is recorded in the save (request, response, usage,
//! latency, cost); `LOAM_RECORD=0` turns recording off.
//!
//! Variables such as `ANTHROPIC_API_KEY` can live in a `.env` file, read from
//! the current directory and then the repo root; real environment variables
//! win.

use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use loam::commands::{self, Notice};
use loam::game::{Game, GameError};
use loam::llm::AnthropicClient;
use loam::tui::App;
use loam::worker::{Command as WorkerCommand, Event as WorkerEvent, Worker};
use loam::world::World;
use loam::wrap::{Wrapper, output_margin, output_width, wrap_with_margin};

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
    let tui = use_tui(
        args.plain,
        std::env::var("LOAM_PLAIN").ok().as_deref(),
        io::stdout().is_terminal(),
    );
    let client = AnthropicClient::from_env()?;
    let mut game = Game::new(World::open(&path)?);
    game.set_recording(std::env::var("LOAM_RECORD").map_or(true, |v| v.trim() != "0"));
    if seed_file.is_some() && game.has_bible()? {
        note(&format!(
            "[{path} already has a world; the seed is ignored]"
        ));
    }
    let mut editor = LineEditor::new()?;

    // A new world's seed is chosen on plain lines, before the TUI starts.
    let mut intro = Vec::new();
    let seed = if game.has_bible()? {
        None
    } else {
        let mut seed = String::new();
        if let Some(found) = &seed_file {
            intro.push(format!(
                "New world in {path}, seeded from {}.",
                found.source
            ));
            seed.push_str(&found.text);
        } else if let Some(found) = choose_seed(&path, &mut editor)? {
            intro.push(format!("Seeded from {}.", found.source));
            seed.push_str(&found.text);
        } else {
            say("Describe the world's plot, tone and setting; end with a blank line.");
            while let Some(line) = editor.read_line("")? {
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
        Some(seed.trim().to_string())
    };
    let resume = if game.has_begun()? {
        Some(format!("Resuming {path}.\n\n{}", game.look()?))
    } else {
        None
    };

    let worker = Worker::spawn(game, Box::new(client));
    // Inspectors read through a second, read-only connection.
    let view = World::open_read_only(&path)?;
    if tui {
        let mut app = App::new(view, loam::tui::mouse_from_env());
        for line in intro {
            app.info(line);
        }
        if let Some(seed) = seed {
            app.info("Building the world…");
            app.send(WorkerCommand::CreateBible(seed));
        }
        match resume {
            Some(text) => {
                app.load_recent(RESUME_STORY);
                app.info(text);
            }
            None => app.send(WorkerCommand::Begin),
        }
        loam::tui::run(app, worker)?;
        return Ok(());
    }

    for line in intro {
        say(&line);
    }
    if let Some(seed) = seed {
        say("Building the world…");
        worker.send(WorkerCommand::CreateBible(seed));
        if let Some(Err(e)) = await_worker(&worker) {
            return Err(e.into());
        }
    }
    match resume {
        Some(text) => say(&text),
        None => {
            worker.send(WorkerCommand::Begin);
            if let Some(Err(e)) = await_worker(&worker) {
                return Err(e.into());
            }
            println!();
        }
    }

    // `:brain` changes, for this session only; the worker gets a copy.
    let mut brains = loam::llm::Brains::default();
    let mut xray_on = commands::xray_from_env(std::env::var("LOAM_XRAY").ok().as_deref());
    loop {
        println!();
        let Some(line) = editor.read_line(PROMPT)? else {
            break;
        };
        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        if input.starts_with(':') {
            let (cmd, arg) = commands::split(input);
            if let Some(report) = commands::report(&view, cmd, arg) {
                match report {
                    Ok(text) => say(&text),
                    Err(notice) => note(&notice),
                }
                continue;
            }
            match cmd {
                ":quit" | ":q" => break,
                ":tick" => {
                    worker.send(WorkerCommand::Tick);
                    await_worker(&worker);
                }
                ":brain" => {
                    if !arg.is_empty() {
                        if let Err(e) = brains.change(arg) {
                            say(&e);
                            continue;
                        }
                        worker.send(WorkerCommand::SetBrains(brains.clone()));
                    }
                    say(&brains.table());
                }
                ":xray" => match commands::xray_switch(arg, xray_on) {
                    Ok(on) => {
                        xray_on = on;
                        say(if on {
                            "X-ray on: each turn ends with its calls, cost, refusals, rules and rolls."
                        } else {
                            "X-ray off."
                        });
                    }
                    Err(usage) => say(usage),
                },
                ":context" => match commands::context(&view, arg) {
                    Ok(commands::ContextReport::Text(text)) => say(&text),
                    Ok(commands::ContextReport::Call { seq, n, call, raw }) => {
                        match commands::call_text(&view, seq, n, &call, raw) {
                            Ok(text) => page(&text),
                            Err(notice) => note(&notice),
                        }
                    }
                    Err(notice) => note(&notice),
                },
                ":help" => say(commands::HELP),
                _ => say(&format!("Unknown command {cmd}. {}", commands::HELP)),
            }
            continue;
        }
        println!();
        let before = loam::xray::latest_turn(&view).ok().flatten();
        worker.send(WorkerCommand::Turn(input.to_string()));
        await_worker(&worker);
        if let Some(footer) = commands::xray_footer(&view, before, xray_on) {
            note(&format!("\n{footer}"));
        }
    }
    // The writer must close last so it checkpoints the WAL into the save.
    drop(view);
    worker.shut_down();
    Ok(())
}

/// Story paragraphs a resumed save opens with in the TUI.
const RESUME_STORY: usize = 20;

/// The TUI unless `--plain`, `LOAM_PLAIN=1` or stdout isn't a terminal.
fn use_tui(plain_flag: bool, plain_env: Option<&str>, terminal: bool) -> bool {
    !plain_flag && plain_env.is_none_or(|v| v.trim() != "1") && terminal
}

/// Plain mode: print what the worker reports until it is idle again:
/// narration streamed and word-wrapped, then the engine's notices. Returns
/// how building the bible or the opening went, if that is what ran.
fn await_worker(worker: &Worker) -> Option<Result<(), GameError>> {
    let mut wrapper = Wrapper::with_margin(output_width(), output_margin());
    let mut out = io::stdout();
    let mut fatal = None;
    while let Some(event) = worker.recv() {
        match event {
            WorkerEvent::Text(t) => {
                let _ = write!(out, "{}", wrapper.push(&t));
                let _ = out.flush();
            }
            WorkerEvent::BibleDone(result) => fatal = Some(result),
            WorkerEvent::TurnDone { opening, outcome } => {
                let _ = write!(out, "{}", wrapper.finish());
                let _ = out.flush();
                if opening {
                    fatal = Some(outcome.map(|_| ()));
                    continue;
                }
                let notices = commands::turn_notices(&outcome);
                for n in &notices {
                    if let Notice::Engine(t) = n {
                        note(&format!("\n{t}"));
                    }
                }
                if outcome.is_ok() {
                    println!();
                }
                for n in &notices {
                    if let Notice::Story(t) = n {
                        say(&format!("\n{t}"));
                    }
                }
            }
            WorkerEvent::TickDone { outcome, .. } => note(&commands::tick_notice(&outcome)),
            WorkerEvent::RerollProgress { .. } => {}
            WorkerEvent::Ready => break,
        }
    }
    fatal
}

const USAGE: &str = "Usage: loam [--plain] [--seed <name|file> | --prompt <text>] [save.db]
       loam --list-seeds

  A new save without --seed or --prompt asks you to pick a named seed or
  write your own.

  --seed <name|file>  Seed a new save from a file, from seeds/<name>.txt, or
                      from the seed of that name built into loam.
                      The save defaults to <name>.db (otherwise loam.db).
                      Ignored when the save already has a world.
  --prompt <text>     Seed a new save from your own prompt. The save
                      defaults to loam.db.
  --list-seeds        List the named seed prompts (./seeds and built-in).
  --plain             Use the line REPL instead of the full-screen TUI (also
                      LOAM_PLAIN=1, or when stdout isn't a terminal).

  In the TUI the mouse is captured (wheel scrolls, click focuses);
  LOAM_MOUSE=0 or :mouse off restores native text selection.

  LOAM_XRAY=1 starts plain mode with the X-ray footer on (:xray toggles it).

  Text wraps at 80 columns or the terminal width, whichever is narrower;
  set LOAM_WIDTH to another width, or to 0 to turn wrapping off.";

enum Command {
    Help,
    ListSeeds,
    Play(PlayArgs),
}

#[derive(Debug, PartialEq)]
struct PlayArgs {
    save: String,
    plain: bool,
    seed: Option<String>,
    prompt: Option<String>,
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Command, String> {
    let mut seed = None;
    let mut prompt = None;
    let mut save = None;
    let mut plain = false;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(Command::Help),
            "--list-seeds" => return Ok(Command::ListSeeds),
            "--plain" => plain = true,
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
    Ok(Command::Play(PlayArgs {
        save,
        plain,
        seed,
        prompt,
    }))
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
    input: &mut LineEditor,
) -> Result<Option<Seed>, Box<dyn std::error::Error>> {
    let dirs = seed_dirs();
    let names = list_seeds(&dirs);
    say(&format!("New world in {path}. Pick a seed:"));
    for (i, name) in names.iter().enumerate() {
        let teaser =
            resolve_seed(name, &dirs).map_or_else(|_| String::new(), |s| seed_teaser(&s.text));
        say(&format!("  {}. {name} — {teaser}", i + 1));
    }
    say(&format!("  {}. Write your own", names.len() + 1));
    loop {
        let Some(answer) = input.read_line(PROMPT)? else {
            return Err("no seed chosen".into());
        };
        match parse_seed_choice(&answer, &names) {
            Some(SeedChoice::Named(name)) => return Ok(Some(resolve_seed(&name, &dirs)?)),
            Some(SeedChoice::Own) => return Ok(None),
            None => say(&format!("Type a number from 1 to {}.", names.len() + 1)),
        }
    }
}

/// Reads input lines through a line editor, so the arrow keys move the caret
/// and recall earlier lines instead of typing escape codes. Piped input is
/// read line by line as before.
struct LineEditor(rustyline::Editor<InputStyle, rustyline::history::DefaultHistory>);

impl LineEditor {
    fn new() -> rustyline::Result<Self> {
        use rustyline::config::{ColorMode, Config};
        // rustyline already skips colour when stdout isn't a terminal.
        let colour = if colour_on(&io::stdout()) {
            ColorMode::Enabled
        } else {
            ColorMode::Disabled
        };
        let mut editor =
            rustyline::Editor::with_config(Config::builder().color_mode(colour).build())?;
        editor.set_helper(Some(InputStyle));
        Ok(Self(editor))
    }

    /// Shows `prompt` and reads one line, keeping it in the history when it
    /// isn't blank. `None` at end of input (Ctrl-D) or on Ctrl-C.
    fn read_line(&mut self, prompt: &str) -> rustyline::Result<Option<String>> {
        use rustyline::error::ReadlineError;
        match self.0.readline(prompt) {
            Ok(line) => {
                if !line.trim().is_empty() {
                    self.0.add_history_entry(line.as_str())?;
                }
                Ok(Some(line))
            }
            Err(ReadlineError::Eof | ReadlineError::Interrupted) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// The REPL prompt. It is as wide as the default margin, so typed text
/// lines up with the output while the glyph sits in the margin.
const PROMPT: &str = "› ";

const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

/// Styles the line being typed: an accent-coloured prompt and bold input, so
/// your lines stand out from the game's in scrollback. rustyline only calls
/// it when colour is on.
struct InputStyle;

impl rustyline::highlight::Highlighter for InputStyle {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> std::borrow::Cow<'l, str> {
        if line.is_empty() {
            line.into()
        } else {
            format!("\x1b[1m{line}{RESET}").into()
        }
    }

    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(
        &'s self,
        prompt: &'p str,
        _default: bool,
    ) -> std::borrow::Cow<'b, str> {
        if prompt.is_empty() {
            prompt.into()
        } else {
            format!("\x1b[1;36m{prompt}{RESET}").into()
        }
    }

    /// Restyle on every keystroke, so typed characters are bold too.
    fn highlight_char(
        &self,
        _line: &str,
        _pos: usize,
        _kind: rustyline::highlight::CmdKind,
    ) -> bool {
        true
    }
}

impl rustyline::completion::Completer for InputStyle {
    type Candidate = String;
}
impl rustyline::hint::Hinter for InputStyle {
    type Hint = String;
}
impl rustyline::validate::Validator for InputStyle {}
impl rustyline::Helper for InputStyle {}

/// Prints text word-wrapped to the output width behind the margin, then a
/// newline.
fn say(text: &str) {
    println!(
        "{}",
        wrap_with_margin(text, output_width(), output_margin())
    );
}

/// Shows long text through `$PAGER` (default `less -FRX`) when stdout is a
/// terminal, else prints it.
fn page(text: &str) {
    if !io::stdout().is_terminal() {
        println!("{text}");
        return;
    }
    let pager = std::env::var("PAGER")
        .ok()
        .filter(|p| !p.trim().is_empty())
        .unwrap_or_else(|| "less -FRX".into());
    let child = std::process::Command::new("sh")
        .arg("-c")
        .arg(&pager)
        .stdin(std::process::Stdio::piped())
        .spawn();
    match child {
        Ok(mut child) => {
            if let Some(mut stdin) = child.stdin.take() {
                // A pager quit early closes the pipe; that's fine.
                let _ = stdin.write_all(text.as_bytes());
                let _ = stdin.write_all(b"\n");
            }
            let _ = child.wait();
        }
        Err(_) => println!("{text}"),
    }
}

/// Prints an engine notice (`[world tick: …]`, failures) on stderr like
/// [`say`], dimmed when stderr is a colour terminal.
fn note(text: &str) {
    let text = wrap_with_margin(text, output_width(), output_margin());
    if colour_on(&io::stderr()) {
        eprintln!("{DIM}{text}{RESET}");
    } else {
        eprintln!("{text}");
    }
}

/// Whether to colour what goes to `stream`: it's a terminal and `NO_COLOR`
/// isn't set to something (see no-color.org).
fn colour_on(stream: &impl io::IsTerminal) -> bool {
    stream.is_terminal() && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
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
                plain: false,
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
    fn plain_flag_and_env_choose_the_line_repl() {
        assert!(play(&["--plain", "x.db"]).plain);
        assert!(use_tui(false, None, true));
        assert!(use_tui(false, Some("0"), true));
        assert!(!use_tui(true, None, true));
        assert!(!use_tui(false, Some("1"), true));
        assert!(!use_tui(false, None, false));
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
