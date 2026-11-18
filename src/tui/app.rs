//! The TUI's state and what keys, the mouse and worker events do to it.
//!
//! Nothing here touches the terminal: [`App`] is driven by key events and
//! [`Event`]s and drawn by [`super::view`], so tests feed it keys and read
//! its state or a rendered `TestBackend` buffer.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};

use crate::commands::{self, Notice, Recent, Status};
use crate::llm::{Brains, TABLE_HEADER_LINES};
use crate::worker::{Command, Event};
use crate::world::World;

/// Where keys go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Input,
    Story,
    Panel,
}

/// A paragraph in the story pane.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub kind: Kind,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// What the player typed.
    Input,
    /// The game's narration (possibly still streaming).
    Narration,
    /// A dim engine notice.
    Notice,
    /// A plain line from loam itself ("Resuming …").
    Info,
}

/// One view in the side panel's back stack.
#[derive(Debug, Clone, PartialEq)]
pub struct PanelView {
    /// The command that made it (`:rules`), shown as the title.
    pub cmd: String,
    pub arg: String,
    pub text: String,
    /// First visible line.
    pub scroll: usize,
    /// The highlighted line, in views with rows to pick (`:brain`).
    pub selected: Option<usize>,
}

impl PanelView {
    pub fn title(&self) -> String {
        if self.arg.is_empty() {
            self.cmd.clone()
        } else {
            format!("{} {}", self.cmd, self.arg)
        }
    }
}

/// The search being typed after `/`.
#[derive(Debug, Clone, PartialEq)]
pub struct Search {
    pub pane: Focus,
    pub query: String,
}

/// Screen areas from the last draw, for mouse hits and paging.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Areas {
    pub story: Option<Rect>,
    pub panel: Option<Rect>,
    pub input: Rect,
}

/// Below this width the side panel covers the story instead of sitting
/// beside it.
pub const OVERLAY_BELOW: u16 = 120;

pub struct App {
    /// The read-only view of the save.
    view: World,
    /// The last call id before this session, for the session's cost.
    since_call: i64,
    pub status: Status,
    /// Each role's model and effort this session; the worker gets a copy
    /// whenever it changes.
    pub brains: Brains,
    pub story: Vec<Entry>,
    /// Narration is streaming into the last story entry.
    streaming: bool,
    /// Story lines scrolled up from the bottom (0 follows new text).
    pub story_scroll: usize,
    pub focus: Focus,
    pub input: String,
    /// Caret position in `input`, in characters.
    pub cursor: usize,
    history: Vec<String>,
    /// Position while walking the history, and the line being typed before.
    history_at: Option<usize>,
    draft: String,
    pub panel: Vec<PanelView>,
    pub panel_open: bool,
    pub help_open: bool,
    pub search: Option<Search>,
    /// The last search, for `n`/`N` and highlighting.
    pub last_search: Option<(Focus, String)>,
    /// A one-off message in the hint bar, cleared by the next key.
    pub flash: Option<String>,
    /// Commands the worker hasn't finished.
    pending: usize,
    /// Commands for the worker, taken by the event loop.
    pub outbox: Vec<Command>,
    pub mouse: bool,
    pub quit: bool,
    /// Set by the last draw.
    pub areas: Areas,
    pub(crate) story_height: usize,
    pub(crate) story_width: usize,
    pub(crate) panel_height: usize,
    pub(crate) panel_width: usize,
}

impl App {
    /// A TUI over `view`, the read-only connection to the save the worker
    /// plays.
    pub fn new(view: World, mouse: bool) -> App {
        let since_call = view.last_call_id().unwrap_or(0);
        let mut app = App {
            view,
            since_call,
            status: Status::default(),
            brains: Brains::default(),
            story: Vec::new(),
            streaming: false,
            story_scroll: 0,
            focus: Focus::Input,
            input: String::new(),
            cursor: 0,
            history: Vec::new(),
            history_at: None,
            draft: String::new(),
            panel: Vec::new(),
            panel_open: false,
            help_open: false,
            search: None,
            last_search: None,
            flash: None,
            pending: 0,
            outbox: Vec::new(),
            mouse,
            quit: false,
            areas: Areas::default(),
            story_height: 20,
            story_width: 80,
            panel_height: 20,
            panel_width: 40,
        };
        app.refresh_status();
        app
    }

    /// The worker is running something.
    pub fn busy(&self) -> bool {
        self.pending > 0
    }

    /// Ask the worker to do something.
    pub fn send(&mut self, command: Command) {
        self.pending += 1;
        self.outbox.push(command);
    }

    /// Add a plain line from loam to the story.
    pub fn info(&mut self, text: impl Into<String>) {
        self.push_entry(Kind::Info, text.into());
    }

    /// Open the story on the save's recent inputs and narration.
    pub fn load_recent(&mut self, limit: usize) {
        if let Ok(lines) = commands::recent_story(&self.view, limit) {
            for line in lines {
                match line {
                    Recent::Input(t) => self.push_entry(Kind::Input, t),
                    Recent::Narration(t) => self.push_entry(Kind::Narration, t),
                }
            }
        }
    }

    fn push_entry(&mut self, kind: Kind, text: String) {
        self.keep_place(|app| {
            app.streaming = false;
            app.story.push(Entry { kind, text });
        });
    }

    /// Change the story without moving what a player who scrolled up is
    /// reading: new lines at the bottom add to the scroll.
    fn keep_place(&mut self, change: impl FnOnce(&mut Self)) {
        if self.story_scroll == 0 {
            change(self);
            return;
        }
        let before = self.story_lines(self.story_width).len();
        change(self);
        let after = self.story_lines(self.story_width).len();
        self.story_scroll += after.saturating_sub(before);
    }

    fn notice(&mut self, text: String) {
        self.push_entry(Kind::Notice, text);
    }

    fn refresh_status(&mut self) {
        if let Ok(s) = commands::status(&self.view, self.since_call) {
            self.status = s;
        }
    }

    /// The side panel covers the story at this width.
    pub fn panel_overlays(&self, width: u16) -> bool {
        width < OVERLAY_BELOW
    }

    /// Take what the worker reported.
    pub fn on_event(&mut self, event: Event) {
        match event {
            Event::Text(t) => self.keep_place(|app| {
                if !app.streaming {
                    app.story.push(Entry {
                        kind: Kind::Narration,
                        text: String::new(),
                    });
                    app.streaming = true;
                }
                if let Some(last) = app.story.last_mut() {
                    last.text.push_str(&t);
                }
            }),
            Event::BibleDone(Ok(())) => self.info("The world is built."),
            Event::BibleDone(Err(e)) => self.notice(format!("[building the world failed: {e}]")),
            Event::TurnDone { opening, outcome } => {
                self.streaming = false;
                if opening && let Err(e) = &outcome {
                    self.notice(format!("[the opening failed: {e}]"));
                } else {
                    for n in commands::turn_notices(&outcome) {
                        let (Notice::Engine(t) | Notice::Story(t)) = n;
                        self.notice(t);
                    }
                }
                self.refresh_status();
                self.refresh_panel();
            }
            Event::TickDone { forced, outcome } => {
                self.notice(commands::tick_notice(&outcome));
                if forced && let Some(v) = self.panel.iter_mut().rev().find(|v| v.cmd == ":tick") {
                    v.text = commands::tick_report(&outcome);
                }
                self.refresh_status();
                self.refresh_panel();
            }
            Event::RerollProgress { .. } => {}
            Event::Ready => {
                self.pending = self.pending.saturating_sub(1);
                self.streaming = false;
            }
        }
    }

    /// Re-run the top panel view's report, so it shows the current state.
    fn refresh_panel(&mut self) {
        let Some(top) = self.panel.last() else {
            return;
        };
        if let Some(Ok(text)) = commands::report(&self.view, &top.cmd, &top.arg)
            && let Some(top) = self.panel.last_mut()
        {
            top.text = text;
        }
    }

    /// Push a view onto the panel and show it.
    pub fn open_view(&mut self, cmd: &str, arg: &str, text: String) {
        let view = PanelView {
            cmd: cmd.to_string(),
            arg: arg.to_string(),
            text,
            scroll: 0,
            selected: None,
        };
        // The same report again replaces itself rather than stacking.
        if self
            .panel
            .last()
            .is_some_and(|v| v.cmd == view.cmd && v.arg == view.arg)
        {
            self.panel.pop();
        }
        self.panel.push(view);
        self.panel_open = true;
    }

    /// Open (or redraw) the `:brain` table with `row` selected.
    fn show_brains(&mut self, row: usize) {
        self.open_view(":brain", "", self.brains.table());
        if let Some(v) = self.panel.last_mut() {
            v.selected = Some(TABLE_HEADER_LINES + row);
        }
    }

    /// The selected role's row, while the `:brain` view is on top.
    fn brain_row(&self) -> Option<usize> {
        let top = self.panel.last().filter(|v| v.cmd == ":brain")?;
        top.selected?.checked_sub(TABLE_HEADER_LINES)
    }

    /// Hand the worker a copy of the changed brains.
    fn brains_changed(&mut self) {
        // Not `send`: the worker reports nothing for it, so it isn't pending.
        self.outbox.push(Command::SetBrains(self.brains.clone()));
    }

    /// Keys in the `:brain` view; `false` when the key isn't one of them.
    fn brain_key(&mut self, key: KeyEvent) -> bool {
        let Some(row) = self.brain_row() else {
            return false;
        };
        let rows = self.brains.iter().count();
        let row = match key.code {
            KeyCode::Up | KeyCode::Char('k') => row.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => (row + 1).min(rows - 1),
            KeyCode::Enter | KeyCode::Char('e') => {
                let role = self.brains.role_at(row).expect("a row per role");
                if key.code == KeyCode::Enter {
                    self.brains.cycle_model(role);
                } else {
                    self.brains.cycle_effort(role);
                }
                self.brains_changed();
                row
            }
            _ => return false,
        };
        self.show_brains(row);
        true
    }

    /// Esc or `h` in the panel: back one view; past the first, close it.
    fn panel_back(&mut self) {
        self.panel.pop();
        if self.panel.is_empty() {
            self.panel_open = false;
        }
        if !self.panel_open {
            self.focus = Focus::Input;
        }
    }

    fn toggle_panel(&mut self) {
        self.panel_open = !self.panel_open;
        if !self.panel_open && self.focus == Focus::Panel {
            self.focus = Focus::Input;
        }
    }

    /// Run what was typed on the input line.
    pub fn submit(&mut self) {
        let line = std::mem::take(&mut self.input);
        self.cursor = 0;
        self.history_at = None;
        let line = line.trim().to_string();
        if line.is_empty() {
            return;
        }
        if self.history.last() != Some(&line) {
            self.history.push(line.clone());
        }
        if line.starts_with(':') {
            self.command(&line);
            return;
        }
        if self.busy() {
            self.flash = Some("The world is still answering; wait for it.".into());
            self.input = line;
            self.cursor = self.input.chars().count();
            return;
        }
        self.push_entry(Kind::Input, line.clone());
        self.story_scroll = 0;
        self.send(Command::Turn(line));
    }

    fn command(&mut self, line: &str) {
        let (cmd, arg) = commands::split(line);
        if let Some(report) = commands::report(&self.view, cmd, arg) {
            match report {
                Ok(text) => self.open_view(cmd, arg, text),
                Err(notice) => self.flash = Some(notice),
            }
            return;
        }
        match cmd {
            ":quit" | ":q" => self.quit = true,
            ":help" => self.open_view(":help", "", help_text()),
            ":panel" => self.toggle_panel(),
            ":brain" if arg.is_empty() => {
                self.show_brains(0);
                self.focus = Focus::Panel;
            }
            ":brain" => match self.brains.change(arg) {
                Ok(()) => {
                    self.brains_changed();
                    let row = self.brain_row().unwrap_or(0);
                    self.show_brains(row);
                }
                Err(e) => self.flash = Some(e),
            },
            ":mouse" => match arg {
                "on" => self.mouse = true,
                "off" => self.mouse = false,
                "" => self.mouse = !self.mouse,
                _ => self.flash = Some("Usage: :mouse [on|off]".into()),
            },
            ":tick" if self.busy() => {
                self.flash = Some("The world is still answering; wait for it.".into());
            }
            ":tick" => {
                self.open_view(":tick", "", "Ticking…".into());
                self.send(Command::Tick);
            }
            _ => self.flash = Some(format!("Unknown command {cmd}. :help lists them.")),
        }
    }

    /// A key press.
    pub fn on_key(&mut self, key: KeyEvent) {
        self.flash = None;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // Keys that work everywhere.
        match key.code {
            KeyCode::Char('c' | 'd') if ctrl => {
                self.quit = true;
                return;
            }
            KeyCode::Char('l') if ctrl => {
                self.command(":log");
                return;
            }
            KeyCode::Char('p') if ctrl => {
                self.toggle_panel();
                return;
            }
            KeyCode::Char(c @ ('x' | 'r' | 'b')) if ctrl => {
                let what = match c {
                    'x' => "The X-ray panel",
                    'r' => "Rerolls",
                    _ => "The branch tree",
                };
                self.flash = Some(format!("{what} isn't built yet."));
                return;
            }
            KeyCode::PageUp => {
                self.scroll_story_up(self.story_height.saturating_sub(1).max(1));
                return;
            }
            KeyCode::PageDown => {
                self.scroll_story_down(self.story_height.saturating_sub(1).max(1));
                return;
            }
            _ => {}
        }
        if self.search.is_some() {
            self.search_key(key);
            return;
        }
        if self.help_open {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('?' | 'q')) {
                self.help_open = false;
            }
            return;
        }
        match key.code {
            KeyCode::Tab if self.focus == Focus::Input && self.completing() => self.complete(),
            KeyCode::Tab => self.cycle_focus(true),
            KeyCode::BackTab => self.cycle_focus(false),
            _ => match self.focus {
                Focus::Input => self.input_key(key),
                Focus::Story => self.story_key(key),
                Focus::Panel => self.panel_key(key),
            },
        }
    }

    /// Panes Tab cycles through, in order. The story is hidden while the
    /// panel covers it.
    fn focusable(&self) -> Vec<Focus> {
        let overlay = self.panel_open && self.areas.story.is_none() && self.areas.panel.is_some();
        let mut panes = vec![Focus::Input];
        if !overlay {
            panes.push(Focus::Story);
        }
        if self.panel_open {
            panes.push(Focus::Panel);
        }
        panes
    }

    fn cycle_focus(&mut self, forward: bool) {
        let panes = self.focusable();
        let at = panes.iter().position(|&f| f == self.focus).unwrap_or(0);
        let n = panes.len();
        self.focus = panes[if forward {
            (at + 1) % n
        } else {
            (at + n - 1) % n
        }];
    }

    fn input_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Enter => self.submit(),
            KeyCode::Esc => {
                if self.panel_open {
                    self.panel_back();
                } else {
                    self.input.clear();
                    self.cursor = 0;
                }
            }
            KeyCode::Up => self.history_step(true),
            KeyCode::Down => self.history_step(false),
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.input.chars().count()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.chars().count(),
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = self.input.chars().count(),
            KeyCode::Char('u') if ctrl => {
                let at = self.byte_at(self.cursor);
                self.input.drain(..at);
                self.cursor = 0;
            }
            KeyCode::Char('w') if ctrl => {
                let before: Vec<char> = self.input.chars().take(self.cursor).collect();
                let mut start = before.len();
                while start > 0 && before[start - 1] == ' ' {
                    start -= 1;
                }
                while start > 0 && before[start - 1] != ' ' {
                    start -= 1;
                }
                let (a, b) = (self.byte_at(start), self.byte_at(self.cursor));
                self.input.drain(a..b);
                self.cursor = start;
            }
            KeyCode::Backspace if self.cursor > 0 => {
                let at = self.byte_at(self.cursor - 1);
                self.input.remove(at);
                self.cursor -= 1;
            }
            KeyCode::Delete if self.cursor < self.input.chars().count() => {
                let at = self.byte_at(self.cursor);
                self.input.remove(at);
            }
            KeyCode::Char(c) if !ctrl => self.insert(&c.to_string()),
            _ => {}
        }
    }

    /// Pasted text goes into the input line, newlines as spaces.
    pub fn paste(&mut self, text: &str) {
        if self.focus == Focus::Input && self.search.is_none() {
            self.insert(&text.replace(['\r', '\n'], " "));
        }
    }

    fn insert(&mut self, s: &str) {
        let at = self.byte_at(self.cursor);
        self.input.insert_str(at, s);
        self.cursor += s.chars().count();
        self.history_at = None;
    }

    fn byte_at(&self, chars: usize) -> usize {
        self.input
            .char_indices()
            .nth(chars)
            .map_or(self.input.len(), |(i, _)| i)
    }

    fn history_step(&mut self, back: bool) {
        if self.history.is_empty() {
            return;
        }
        let next = match (self.history_at, back) {
            (None, true) => {
                self.draft = self.input.clone();
                Some(self.history.len() - 1)
            }
            (None, false) => return,
            (Some(i), true) => Some(i.saturating_sub(1)),
            (Some(i), false) if i + 1 < self.history.len() => Some(i + 1),
            (Some(_), false) => None,
        };
        self.history_at = next;
        self.input = match next {
            Some(i) => self.history[i].clone(),
            None => std::mem::take(&mut self.draft),
        };
        self.cursor = self.input.chars().count();
    }

    /// Tab completes while the line is a bare `:word`.
    fn completing(&self) -> bool {
        self.input.starts_with(':') && !self.input.contains(char::is_whitespace)
    }

    fn complete(&mut self) {
        let matches = commands::complete(&self.input);
        match matches.as_slice() {
            [] => self.flash = Some(format!("No command starts with {}.", self.input)),
            [only] => {
                self.input = format!("{only} ");
                self.cursor = self.input.chars().count();
            }
            many => {
                let first = many[0];
                let common = many.iter().fold(first.len(), |n, m| {
                    first
                        .bytes()
                        .zip(m.bytes())
                        .take(n)
                        .take_while(|(a, b)| a == b)
                        .count()
                });
                self.input = first[..common].to_string();
                self.cursor = self.input.chars().count();
                self.flash = Some(many.join("  "));
            }
        }
    }

    fn story_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('h') => self.focus = Focus::Input,
            KeyCode::Up | KeyCode::Char('k') => self.scroll_story_up(1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_story_down(1),
            KeyCode::Char('g') => self.story_scroll = usize::MAX,
            KeyCode::Char('G') => self.story_scroll = 0,
            KeyCode::Char('/') => self.start_search(Focus::Story),
            KeyCode::Char('n') => self.search_again(Focus::Story, false),
            KeyCode::Char('N') => self.search_again(Focus::Story, true),
            KeyCode::Char('?') => self.help_open = true,
            _ => {}
        }
    }

    fn panel_key(&mut self, key: KeyEvent) {
        if self.brain_key(key) {
            return;
        }
        let height = self.panel_height.max(1);
        match key.code {
            KeyCode::Esc | KeyCode::Char('h') | KeyCode::Left => self.panel_back(),
            KeyCode::Up | KeyCode::Char('k') => self.scroll_panel(-1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_panel(1),
            KeyCode::Char('g') => self.scroll_panel(isize::MIN),
            KeyCode::Char('G') => self.scroll_panel(isize::MAX),
            KeyCode::Char(' ') => self.scroll_panel(height as isize - 1),
            KeyCode::Char('/') => self.start_search(Focus::Panel),
            KeyCode::Char('n') => self.search_again(Focus::Panel, true),
            KeyCode::Char('N') => self.search_again(Focus::Panel, false),
            KeyCode::Char('?') => self.help_open = true,
            // Enter opens the selected item in views that have items; the
            // text reports have none yet.
            _ => {}
        }
    }

    pub fn scroll_story_up(&mut self, n: usize) {
        self.story_scroll = self.story_scroll.saturating_add(n);
    }

    pub fn scroll_story_down(&mut self, n: usize) {
        self.story_scroll = self.story_scroll.saturating_sub(n);
    }

    fn scroll_panel(&mut self, by: isize) {
        let max = self.panel_lines(self.panel_width).len().saturating_sub(1);
        if let Some(v) = self.panel.last_mut() {
            v.scroll = v.scroll.saturating_add_signed(by).min(max);
        }
    }

    fn start_search(&mut self, pane: Focus) {
        self.search = Some(Search {
            pane,
            query: String::new(),
        });
    }

    fn search_key(&mut self, key: KeyEvent) {
        let Some(search) = self.search.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => self.search = None,
            KeyCode::Backspace => {
                search.query.pop();
            }
            KeyCode::Char(c) => search.query.push(c),
            KeyCode::Enter => {
                let Search { pane, query } = self.search.take().unwrap();
                if !query.is_empty() {
                    self.last_search = Some((pane, query));
                    // The story searches back from the bottom, a panel down
                    // from its top.
                    self.search_again(pane, pane == Focus::Panel);
                }
            }
            _ => {}
        }
    }

    /// Jump to the next match of the last search in `pane`: towards the end
    /// with `forward`, else towards the start, wrapping around.
    fn search_again(&mut self, pane: Focus, forward: bool) {
        let Some((p, query)) = self.last_search.clone() else {
            return;
        };
        if p != pane {
            return;
        }
        let q = query.to_lowercase();
        let (lines, top) = match pane {
            Focus::Story => {
                let lines: Vec<String> = self
                    .story_lines(self.story_width)
                    .into_iter()
                    .map(|(t, _)| t)
                    .collect();
                let top = self.story_top(lines.len());
                (lines, top)
            }
            _ => (
                self.panel_lines(self.panel_width),
                self.panel.last().map_or(0, |v| v.scroll),
            ),
        };
        let hits: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.to_lowercase().contains(&q))
            .map(|(i, _)| i)
            .collect();
        let found = if forward {
            hits.iter().find(|&&i| i > top).or(hits.first())
        } else {
            hits.iter().rev().find(|&&i| i < top).or(hits.last())
        };
        let Some(&line) = found else {
            self.flash = Some(format!("No match for {query}."));
            return;
        };
        match pane {
            Focus::Story => {
                let bottom_top = lines.len().saturating_sub(self.story_height);
                self.story_scroll = bottom_top.saturating_sub(line);
            }
            _ => {
                if let Some(v) = self.panel.last_mut() {
                    v.scroll = line;
                }
            }
        }
    }

    /// The first story line on screen, given the wrapped line count.
    pub(crate) fn story_top(&self, total: usize) -> usize {
        let bottom_top = total.saturating_sub(self.story_height);
        bottom_top.saturating_sub(self.story_scroll.min(bottom_top))
    }

    /// The story wrapped at `width`, each line with its kind (`None` for
    /// the blank lines between paragraphs).
    pub fn story_lines(&self, width: usize) -> Vec<(String, Option<Kind>)> {
        let width = text_width(width);
        let mut out: Vec<(String, Option<Kind>)> = Vec::new();
        for (i, entry) in self.story.iter().enumerate() {
            let quiet_run =
                i > 0 && entry.kind == Kind::Notice && self.story[i - 1].kind == Kind::Notice;
            if i > 0 && !quiet_run {
                out.push((String::new(), None));
            }
            let wrapped = crate::wrap::wrap_with_margin(&entry.text, width, MARGIN);
            for (n, line) in wrapped.lines().enumerate() {
                let line = if entry.kind == Kind::Input && n == 0 {
                    format!("{PROMPT}{}", line.get(MARGIN..).unwrap_or(""))
                } else {
                    line.to_string()
                };
                out.push((line, Some(entry.kind)));
            }
        }
        out
    }

    /// The top panel view wrapped at `width`.
    pub fn panel_lines(&self, width: usize) -> Vec<String> {
        match self.panel.last() {
            Some(v) => crate::wrap::wrap(&v.text, width.max(1))
                .lines()
                .map(str::to_string)
                .collect(),
            None => vec![EMPTY_PANEL.to_string()],
        }
    }

    /// The mouse: the wheel scrolls the pane under it, a click focuses it.
    pub fn on_mouse(&mut self, m: MouseEvent) {
        let at = Position::new(m.column, m.row);
        let over = |r: Option<Rect>| r.is_some_and(|r| r.contains(at));
        let pane = if over(self.areas.panel) {
            Some(Focus::Panel)
        } else if over(self.areas.story) {
            Some(Focus::Story)
        } else if self.areas.input.contains(at) {
            Some(Focus::Input)
        } else {
            None
        };
        match (m.kind, pane) {
            (MouseEventKind::ScrollUp, Some(Focus::Panel)) => self.scroll_panel(-3),
            (MouseEventKind::ScrollDown, Some(Focus::Panel)) => self.scroll_panel(3),
            (MouseEventKind::ScrollUp, _) => self.scroll_story_up(3),
            (MouseEventKind::ScrollDown, _) => self.scroll_story_down(3),
            (MouseEventKind::Down(MouseButton::Left), Some(p)) => self.focus = p,
            _ => {}
        }
    }

    /// What the hint bar says for the current focus.
    pub fn hints(&self) -> String {
        if let Some(f) = &self.flash {
            return f.clone();
        }
        if self.search.is_some() {
            return "type to search · Enter find · Esc cancel".into();
        }
        if self.help_open {
            return "Esc close help".into();
        }
        match self.focus {
            Focus::Input => "Enter send · ↑↓ history · :cmd Tab completes · Tab focus · \
                             PgUp/PgDn scroll · Ctrl-P panel · Ctrl-L log · Ctrl-C quit"
                .into(),
            Focus::Story => "j/k scroll · g/G top/bottom · / search · n/N next · Tab focus · \
                             Esc input · ? help"
                .into(),
            Focus::Panel if self.brain_row().is_some() => {
                "↑↓ role · Enter model · e effort · Esc/h back · Tab focus · ? help".into()
            }
            Focus::Panel => "j/k scroll · g/G top/bottom · / search · n/N next · Esc/h back · \
                             Tab focus · ? help"
                .into(),
        }
    }
}

/// The input prompt; it sits in the story's margin.
pub const PROMPT: &str = "› ";
/// Story text's left margin, in columns.
pub const MARGIN: usize = crate::wrap::DEFAULT_MARGIN;

/// What an empty panel says.
pub const EMPTY_PANEL: &str = "Nothing open. Try :rules, :quests, :npcs, :stats or :log.";

/// The story's wrap width for a pane `width` columns wide: like the REPL,
/// at most [`crate::wrap::MAX_WIDTH`] unless `LOAM_WIDTH` sets another cap
/// (0 for the whole pane).
fn text_width(width: usize) -> usize {
    let cap = std::env::var("LOAM_WIDTH")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .map_or(
            crate::wrap::MAX_WIDTH,
            |w| if w == 0 { usize::MAX } else { w },
        );
    width.min(cap).max(MARGIN + 1)
}

/// `:help` in the TUI: the commands and where the keys are.
pub fn help_text() -> String {
    format!(
        "{}\n\nTUI: :panel toggles this panel (Ctrl-P), :mouse [on|off] switches mouse \
         capture (off restores native text selection; Shift-drag selects while it's on). \
         Press ? outside the input line for the keys.",
        commands::HELP
    )
}

/// The `?` overlay.
pub const KEYS: &str = "Focus
  Tab / Shift-Tab   cycle input → story → panel
  Esc               back; returns focus to the input

Input line
  Enter             send (a line starting with : is a command)
  ↑ ↓               history
  Tab               complete a :command
  Ctrl-A/E/U/W      start, end, clear to start, delete word

Story and panel
  ↑ ↓  j k          scroll
  g G               top, bottom
  /  n N            search, next, previous
  Esc h             back (the panel pops one view)
  ?                 this help

:brain panel
  ↑ ↓  j k          pick a role
  Enter             next model (haiku → sonnet → opus)
  e                 next effort (low → medium → high → xhigh → max)

Anywhere
  PgUp PgDn         scroll the story
  Ctrl-P            toggle the side panel
  Ctrl-L            event log
  Ctrl-X Ctrl-R Ctrl-B   X-ray, reroll, branches (coming)
  Ctrl-C Ctrl-D     quit

Mouse: wheel scrolls, click focuses. :mouse off (or LOAM_MOUSE=0)
restores native text selection.";

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::game::tests::begun;
    use crossterm::event::KeyEventKind;

    pub(crate) fn app() -> App {
        let (game, _) = begun();
        App::new(game.into_world(), true)
    }

    pub(crate) fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    pub(crate) fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new_with_kind(KeyCode::Char(c), KeyModifiers::CONTROL, KeyEventKind::Press)
    }

    pub(crate) fn typed(app: &mut App, text: &str) {
        for c in text.chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
    }

    #[test]
    fn typing_and_enter_sends_a_turn_and_records_history() {
        let mut app = app();
        typed(&mut app, "open the door");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.outbox, [Command::Turn("open the door".into())]);
        assert!(app.busy());
        assert_eq!(app.story.last().unwrap().kind, Kind::Input);
        assert!(app.input.is_empty());
        app.on_key(key(KeyCode::Up));
        assert_eq!(app.input, "open the door");
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.input, "");
    }

    #[test]
    fn brain_changes_by_command_and_by_key_reach_the_worker() {
        use crate::llm::{Brains, Effort, Model};
        let mut app = app();
        typed(&mut app, ":brain director=opus effort=high");
        app.on_key(key(KeyCode::Enter));
        let mut want = Brains::default();
        want.change("director=opus effort=high").unwrap();
        assert_eq!(app.outbox, [Command::SetBrains(want.clone())]);
        assert!(!app.busy(), "SetBrains has no Ready to wait for");
        assert_eq!(app.panel.last().unwrap().cmd, ":brain");

        // A bad change is reported and changes nothing.
        typed(&mut app, ":brain director=gpt");
        app.on_key(key(KeyCode::Enter));
        assert!(app.flash.as_deref().unwrap().starts_with("Unknown model"));
        assert_eq!(app.brains, want);
        assert_eq!(app.outbox.len(), 1);

        // Bare :brain focuses the table; ↓ picks the narrator, Enter and e
        // cycle its model and effort.
        typed(&mut app, ":brain");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.focus, Focus::Panel);
        assert!(app.hints().contains("Enter model"));
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Enter));
        app.on_key(key(KeyCode::Char('e')));
        let narrator = app.brains.get("narrator").unwrap();
        assert_eq!(narrator.model, Model::Opus);
        assert_eq!(narrator.effort, Some(Effort::Low));
        assert_eq!(
            app.outbox.last(),
            Some(&Command::SetBrains(app.brains.clone()))
        );
        // ↑ past the first row stays there; Esc leaves the view.
        for _ in 0..3 {
            app.on_key(key(KeyCode::Up));
        }
        assert_eq!(app.brain_row(), Some(0));
        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.brain_row(), None);
    }

    #[test]
    fn a_turn_is_refused_while_one_runs_but_commands_still_work() {
        let mut app = app();
        app.send(Command::Turn("wait".into()));
        typed(&mut app, "look");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.outbox.len(), 1);
        assert!(app.flash.as_deref().unwrap().contains("still answering"));
        assert_eq!(app.input, "look");
        app.input.clear();
        app.cursor = 0;
        typed(&mut app, ":rules");
        app.on_key(key(KeyCode::Enter));
        assert!(app.panel_open);
        assert_eq!(app.panel.last().unwrap().cmd, ":rules");
    }

    #[test]
    fn streamed_text_builds_one_narration_then_notices_follow() {
        let mut app = app();
        app.send(Command::Turn("x".into()));
        app.on_event(Event::Text("The door ".into()));
        app.on_event(Event::Text("creaks.".into()));
        app.on_event(Event::TurnDone {
            opening: false,
            outcome: Err(crate::game::GameError::NotStarted),
        });
        app.on_event(Event::Ready);
        let n = app.story.len();
        assert_eq!(app.story[n - 2].text, "The door creaks.");
        assert_eq!(app.story[n - 1].kind, Kind::Notice);
        assert!(app.story[n - 1].text.starts_with("[turn failed"));
        assert!(!app.busy());
    }

    #[test]
    fn tab_cycles_focus_and_esc_returns_to_input() {
        let mut app = app();
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Story);
        // No panel open: back to the input.
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Input);
        app.on_key(ctrl('p'));
        app.on_key(key(KeyCode::Tab));
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Panel);
        app.on_key(key(KeyCode::BackTab));
        assert_eq!(app.focus, Focus::Story);
        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.focus, Focus::Input);
    }

    #[test]
    fn tab_completes_a_bare_command() {
        let mut app = app();
        typed(&mut app, ":ru");
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.input, ":rules ");
        assert_eq!(app.focus, Focus::Input);
        app.input = ":qu".into();
        app.cursor = 3;
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.input, ":qu");
        assert!(app.flash.as_deref().unwrap().contains(":quests"));
        // With an argument, Tab moves focus instead.
        app.input = ":why x".into();
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Story);
    }

    #[test]
    fn the_panel_keeps_a_back_stack() {
        let mut app = app();
        for cmd in [":rules", ":quests", ":quests"] {
            typed(&mut app, cmd);
            app.on_key(key(KeyCode::Enter));
        }
        assert_eq!(app.panel.len(), 2);
        app.focus = Focus::Panel;
        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.panel.last().unwrap().cmd, ":rules");
        assert_eq!(app.focus, Focus::Panel);
        app.on_key(key(KeyCode::Char('h')));
        assert!(!app.panel_open);
        assert_eq!(app.focus, Focus::Input);
    }

    #[test]
    fn ctrl_l_opens_the_log_and_reserved_keys_explain_themselves() {
        let mut app = app();
        app.on_key(ctrl('l'));
        assert_eq!(app.panel.last().unwrap().cmd, ":log");
        app.on_key(ctrl('x'));
        assert!(app.flash.as_deref().unwrap().contains("X-ray"));
        assert!(app.outbox.is_empty());
    }

    #[test]
    fn ctrl_c_and_quit_end_the_session() {
        let mut app = app();
        app.on_key(ctrl('c'));
        assert!(app.quit);
        let mut app = super::tests::app();
        typed(&mut app, ":q");
        app.on_key(key(KeyCode::Enter));
        assert!(app.quit);
    }

    #[test]
    fn tick_opens_a_view_filled_in_when_it_finishes() {
        let mut app = app();
        typed(&mut app, ":tick");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.outbox, [Command::Tick]);
        assert_eq!(app.panel.last().unwrap().text, "Ticking…");
        app.on_event(Event::TickDone {
            forced: true,
            outcome: Err(crate::game::GameError::NotStarted),
        });
        assert!(app.panel.last().unwrap().text.contains("world tick failed"));
        assert!(app.story.last().unwrap().text.contains("world tick failed"));
    }

    #[test]
    fn story_scrolls_and_g_jumps() {
        let mut app = app();
        for i in 0..50 {
            app.info(format!("line {i}"));
        }
        app.story_height = 10;
        app.focus = Focus::Story;
        app.on_key(key(KeyCode::Char('k')));
        assert_eq!(app.story_scroll, 1);
        app.on_key(key(KeyCode::PageUp));
        assert_eq!(app.story_scroll, 10);
        app.on_key(key(KeyCode::Char('G')));
        assert_eq!(app.story_scroll, 0);
        app.on_key(key(KeyCode::Char('g')));
        let total = app.story_lines(80).len();
        assert_eq!(app.story_top(total), 0);
    }

    #[test]
    fn streaming_text_keeps_a_scrolled_reader_in_place() {
        let mut app = app();
        for i in 0..30 {
            app.info(format!("line {i}"));
        }
        app.story_height = 10;
        app.story_scroll = 5;
        let lines = app.story_lines(app.story_width);
        let top = app.story_lines(app.story_width)[app.story_top(lines.len())].clone();
        app.on_event(Event::Text("new\nlines\narrive".into()));
        let lines = app.story_lines(app.story_width);
        assert_eq!(lines[app.story_top(lines.len())], top);
    }

    #[test]
    fn search_finds_older_story_lines() {
        let mut app = app();
        app.info("the heron watches");
        for i in 0..40 {
            app.info(format!("filler {i}"));
        }
        app.story_height = 10;
        app.focus = Focus::Story;
        app.on_key(key(KeyCode::Char('/')));
        typed(&mut app, "heron");
        app.on_key(key(KeyCode::Enter));
        let lines = app.story_lines(app.story_width);
        let top = app.story_top(lines.len());
        assert!(lines[top].0.contains("heron"), "{:?}", lines[top]);
        app.on_key(key(KeyCode::Char('/')));
        typed(&mut app, "nothing like this");
        app.on_key(key(KeyCode::Enter));
        assert!(app.flash.as_deref().unwrap().starts_with("No match"));
    }

    #[test]
    fn mouse_command_toggles_capture() {
        let mut app = app();
        typed(&mut app, ":mouse off");
        app.on_key(key(KeyCode::Enter));
        assert!(!app.mouse);
        typed(&mut app, ":mouse");
        app.on_key(key(KeyCode::Enter));
        assert!(app.mouse);
    }

    #[test]
    fn line_editing_keys() {
        let mut app = app();
        typed(&mut app, "take the lamp");
        app.on_key(ctrl('w'));
        assert_eq!(app.input, "take the ");
        app.on_key(key(KeyCode::Home));
        typed(&mut app, "x");
        assert_eq!(app.input, "xtake the ");
        app.on_key(key(KeyCode::Delete));
        assert_eq!(app.input, "xake the ");
        app.on_key(ctrl('e'));
        app.on_key(key(KeyCode::Backspace));
        app.on_key(ctrl('u'));
        assert_eq!(app.input, "");
    }

    #[test]
    fn resuming_loads_recent_story() {
        let mut app = app();
        app.load_recent(10);
        assert_eq!(app.story[0].text, "You wake in a flooded chapel.");
    }
}
