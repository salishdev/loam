//! The full-screen terminal UI (ratatui + crossterm).
//!
//! The game plays on a [`Worker`] thread; [`App`] holds what is on screen and
//! reads the save through a read-only connection, so panels can be browsed
//! and the story scrolled while a turn runs. [`run`] owns the terminal:
//! raw mode, the alternate screen and mouse capture, restored on exit and on
//! panic. [`edit_in_editor`] suspends the TUI for `$EDITOR`.

pub mod app;
pub mod view;

use std::io::{self, Stdout, Write};
use std::path::Path;
use std::time::Duration;

use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event as TermEvent, KeyEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

pub use app::App;
use app::TermAction;

use crate::worker::Worker;

pub type Term = Terminal<CrosstermBackend<Stdout>>;

/// Whether mouse capture starts on: yes unless `LOAM_MOUSE=0`.
pub fn mouse_from_env() -> bool {
    std::env::var("LOAM_MOUSE").map_or(true, |v| v.trim() != "0")
}

/// Run the TUI until the player quits. The terminal is restored however
/// this returns.
pub fn run(mut app: App, worker: Worker) -> io::Result<()> {
    let mut terminal = enter(app.mouse)?;
    install_panic_hook();
    let result = event_loop(&mut terminal, &mut app, &worker);
    leave(&mut terminal);
    // An idle worker closes the save cleanly; one mid-turn is left to the
    // process exit (every event is already committed). The read-only view
    // closes first: the last connection to close checkpoints the WAL into
    // the save, and a read-only one can't.
    let busy = app.busy();
    drop(app);
    if !busy {
        worker.shut_down();
    }
    result
}

fn event_loop(terminal: &mut Term, app: &mut App, worker: &Worker) -> io::Result<()> {
    let mut mouse = app.mouse;
    let mut dirty = true;
    loop {
        while let Some(e) = worker.try_recv() {
            app.on_event(e);
            dirty = true;
        }
        for command in app.outbox.drain(..) {
            worker.send(command);
        }
        if app.mouse != mouse {
            mouse = app.mouse;
            if mouse {
                execute!(io::stdout(), EnableMouseCapture)?;
            } else {
                execute!(io::stdout(), DisableMouseCapture)?;
            }
        }
        if app.quit {
            return Ok(());
        }
        match app.action.take() {
            Some(TermAction::Copy(text)) => {
                let mut out = io::stdout();
                out.write_all(osc52(&text).as_bytes())?;
                out.flush()?;
            }
            Some(TermAction::Edit(text)) => {
                if let Ok(None) | Err(_) = edit_in_editor(terminal, &text, ".json", app.mouse) {
                    app.flash = Some(format!("{} exited with an error.", editor_command()));
                }
                dirty = true;
            }
            Some(TermAction::Surgery(spec, prepared)) => {
                let edited = edit_in_editor(terminal, prepared.text(), ".md", app.mouse)
                    .ok()
                    .flatten();
                app.surgery_edited(spec, prepared, edited);
                dirty = true;
            }
            None => {}
        }
        if dirty {
            terminal.draw(|f| view::draw(f, app))?;
            dirty = false;
        }
        if event::poll(Duration::from_millis(30))? {
            match event::read()? {
                TermEvent::Key(k) if k.kind != KeyEventKind::Release => app.on_key(k),
                TermEvent::Mouse(m) => app.on_mouse(m),
                TermEvent::Paste(text) => app.paste(&text),
                _ => {}
            }
            dirty = true;
        }
    }
}

fn enter(mouse: bool) -> io::Result<Term> {
    enable_raw_mode()?;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen, EnableBracketedPaste)?;
    if mouse {
        execute!(out, EnableMouseCapture)?;
    }
    Terminal::new(CrosstermBackend::new(out))
}

/// Put the terminal back the way the shell had it.
fn restore() {
    let _ = disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen,
        crossterm::cursor::Show
    );
}

fn leave(terminal: &mut Term) {
    restore();
    let _ = terminal.show_cursor();
}

fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        previous(info);
    }));
}

/// Suspend the TUI, let the player edit `text` in `$EDITOR`, then restore
/// the screen. Returns the edited text, or `None` if the editor failed.
/// `suffix` (such as `.md` or `.json`) helps the editor pick a syntax.
pub fn edit_in_editor(
    terminal: &mut Term,
    text: &str,
    suffix: &str,
    mouse: bool,
) -> io::Result<Option<String>> {
    restore();
    let edited = edit_text(text, suffix, &editor_command());
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
    if mouse {
        execute!(io::stdout(), EnableMouseCapture)?;
    }
    terminal.clear()?;
    edited
}

/// The OSC 52 sequence that asks the terminal to put `text` on the
/// clipboard (it works over ssh and inside tmux with `set-clipboard on`).
pub fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes()))
}

/// Standard base64 with padding.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// `$VISUAL`, else `$EDITOR`, else `vi`.
pub fn editor_command() -> String {
    ["VISUAL", "EDITOR"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .find(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "vi".into())
}

/// Write `text` to a temporary file, run `editor` on it through the shell
/// (so `EDITOR="code --wait"` works), and read it back. `None` when the
/// editor exits unsuccessfully.
pub fn edit_text(text: &str, suffix: &str, editor: &str) -> io::Result<Option<String>> {
    let path = std::env::temp_dir().join(format!(
        "loam-edit-{}-{}{suffix}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    ));
    std::fs::File::create(&path)?.write_all(text.as_bytes())?;
    let result = run_editor(editor, &path).and_then(|ok| {
        if ok {
            std::fs::read_to_string(&path).map(Some)
        } else {
            Ok(None)
        }
    });
    let _ = std::fs::remove_file(&path);
    result
}

fn run_editor(editor: &str, path: &Path) -> io::Result<bool> {
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("sh")
        .arg(path)
        .status()?;
    Ok(status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc52_wraps_base64() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(osc52("hi"), "\x1b]52;c;aGk=\x07");
    }

    #[test]
    fn edit_text_runs_the_editor_on_a_temp_file() {
        let edited = edit_text("the door is shut\n", ".txt", "sed -i s/shut/open/").unwrap();
        assert_eq!(edited.as_deref(), Some("the door is open\n"));
        assert_eq!(edit_text("x", ".txt", "false").unwrap(), None);
    }
}
