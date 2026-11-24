//! The TUI's state and what keys, the mouse and worker events do to it.
//!
//! Nothing here touches the terminal: [`App`] is driven by key events and
//! [`Event`]s and drawn by [`super::view`], so tests feed it keys and read
//! its state or a rendered `TestBackend` buffer.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};

use std::path::PathBuf;

use crate::branches::Branch;
use crate::commands::{self, ContextReport, Notice, Recent, Status};
use crate::explain::{self, Notes, Topic};
use crate::knowledge::{Mode, browser};
use crate::llm::{Brains, TABLE_HEADER_LINES};
use crate::reroll::{self, Head, VariantView, surgery};
use crate::worker::{Command, Event};
use crate::world::{StoredCall, World};
use crate::xray::{self, TurnXray, tree::Tree};

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
    /// The highlighted text line, in views with rows to pick (`:brain`,
    /// `:branches`); all of its wrapped screen lines are highlighted.
    pub selected: Option<usize>,
    /// Set for the X-ray view, which shows this instead of `text`.
    pub xray: Option<XrayPanel>,
    /// The compendium mode it was made in (what mode-aware reports, see
    /// [`commands::is_moded`], are read in).
    pub mode: Mode,
    /// Selectable targets from the shared compendium projection.
    pub browser: Option<browser::Page>,
}

/// The X-ray panel's state.
#[derive(Debug, Clone, PartialEq)]
pub struct XrayPanel {
    /// Jump to each new turn as it finishes.
    pub follow: bool,
    /// The turn shown; `None` before any turn is played.
    pub turn: Option<TurnXray>,
    /// The selected call, an index into the turn's calls.
    pub selected: usize,
}

/// The full-screen context viewer: one call as a foldable tree.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextView {
    /// `Turn #12 call 2 · director · sonnet`
    pub title: String,
    /// Shown above the tree when a variant's system prompt changed.
    pub banner: Option<String>,
    pub tree: Tree,
    /// The selected row of `tree.rows()`.
    pub cursor: usize,
    /// First visible screen line.
    pub scroll: usize,
    /// The call's JSON, for `y` and `e`.
    pub raw: String,
    /// The search being typed after `/`.
    pub typing: Option<String>,
    /// The last search, for `n`/`N`.
    pub query: Option<String>,
}

/// The full-screen reroll view: the latest reroll's summary and variants.
#[derive(Debug, Clone, PartialEq)]
pub struct RerollScreen {
    /// `None` until the worker says the reroll started.
    pub head: Option<Head>,
    /// Finished variants, in the order they arrived.
    pub variants: Vec<VariantView>,
    /// The selected variant's number, from 1.
    pub selected: usize,
    pub mode: RerollMode,
    /// First line shown in the narration and diff modes.
    pub scroll: usize,
    /// The worker is still running variants.
    pub running: bool,
    /// Why the reroll failed, if it did.
    pub error: Option<String>,
}

/// What the reroll view shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RerollMode {
    /// Every variant side by side (or as tabs when they don't fit).
    Columns,
    /// The selected variant's whole narration.
    Narration,
    /// The selected variant's full diff, engine bookkeeping included.
    Diff,
}

impl RerollScreen {
    fn new() -> RerollScreen {
        RerollScreen {
            head: None,
            variants: Vec::new(),
            selected: 1,
            mode: RerollMode::Columns,
            scroll: 0,
            running: true,
            error: None,
        }
    }

    /// Variant `n` (from 1), once it finished.
    pub fn variant(&self, n: usize) -> Option<&VariantView> {
        self.variants.iter().find(|v| v.n == n)
    }

    /// How many variants the reroll runs.
    pub fn n(&self) -> usize {
        self.head.as_ref().map_or(0, |h| h.n)
    }
}

/// Where a first-time note shows: at the top of the side panel or of a
/// full-screen view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotePlace {
    Panel,
    Context,
    Reroll,
}

/// Something only the terminal can do, done by the event loop.
#[derive(Debug, Clone, PartialEq)]
pub enum TermAction {
    /// Copy text to the clipboard with OSC 52.
    Copy(String),
    /// Open text in `$EDITOR` (read-only use: edits are discarded).
    Edit(String),
    /// Edit a director prompt in `$EDITOR` for `:surgery`, then hand the
    /// result to [`App::surgery_edited`].
    Surgery(surgery::Spec, surgery::Prepared),
}

impl PanelView {
    pub fn title(&self) -> String {
        let title = if self.arg.is_empty() {
            self.cmd.clone()
        } else {
            format!("{} {}", self.cmd, self.arg)
        };
        if commands::is_moded(&self.cmd) {
            format!("{title} · {}", self.mode.label())
        } else {
            title
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
    /// The last call id before this session (or since switching to this
    /// branch), for the session's cost.
    since_call: i64,
    /// What the session spent in branches it has left.
    cost_carried: f64,
    /// The save being left, until the worker confirms a switch.
    switching_from: Option<PathBuf>,
    /// Said in the story once the switch under way lands (an adoption).
    after_switch: Option<String>,
    /// The rows of the `:branches` view, as last scanned.
    branches: Vec<Branch>,
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
    /// The compendium's mode this session, shared by every mode-aware
    /// view; Player at the start and after switching saves.
    pub mode: Mode,
    pub help_open: bool,
    /// First line of the help overlay on screen.
    pub help_scroll: usize,
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
    /// The context viewer, while open; it covers the screen.
    pub context: Option<ContextView>,
    /// The latest reroll's view, kept after closing for `:reroll show`.
    pub reroll: Option<RerollScreen>,
    /// The reroll view covers the screen.
    pub reroll_open: bool,
    /// A terminal action for the event loop to perform.
    pub action: Option<TermAction>,
    /// The topics this save has explained, this session included.
    explained: Notes,
    /// First-time notes waiting to be dismissed, oldest first.
    pub notes: Vec<(NotePlace, Topic)>,
    /// Set by the last draw.
    pub areas: Areas,
    pub(crate) story_height: usize,
    pub(crate) story_width: usize,
    pub(crate) panel_height: usize,
    pub(crate) panel_width: usize,
    pub(crate) context_height: usize,
}

impl App {
    /// A TUI over `view`, the read-only connection to the save the worker
    /// plays.
    pub fn new(view: World, mouse: bool) -> App {
        let since_call = view.last_call_id().unwrap_or(0);
        let mut app = App {
            view,
            since_call,
            cost_carried: 0.0,
            switching_from: None,
            after_switch: None,
            branches: Vec::new(),
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
            mode: Mode::Player,
            help_open: false,
            help_scroll: 0,
            search: None,
            last_search: None,
            flash: None,
            pending: 0,
            outbox: Vec::new(),
            mouse,
            quit: false,
            context: None,
            reroll: None,
            reroll_open: false,
            action: None,
            explained: Notes::default(),
            notes: Vec::new(),
            areas: Areas::default(),
            story_height: 20,
            story_width: 80,
            panel_height: 20,
            panel_width: 40,
            context_height: 20,
        };
        app.explained = Notes::load(&app.view);
        app.refresh_status();
        app
    }

    /// Queue the notes of `topics` this save hasn't shown yet at `place`,
    /// and have the worker record them as shown.
    fn explain_first(&mut self, place: NotePlace, topics: impl IntoIterator<Item = Topic>) {
        let fresh = self.explained.fresh(topics);
        if fresh.is_empty() {
            return;
        }
        self.notes.extend(fresh.iter().map(|t| (place, *t)));
        // Not `send`: the worker reports nothing for it.
        self.outbox.push(Command::Explained(fresh));
    }

    /// The place on top of the screen that can show a note.
    fn note_place(&self) -> Option<NotePlace> {
        if self.help_open {
            None
        } else if self.context.is_some() {
            Some(NotePlace::Context)
        } else if self.reroll_open && self.reroll.is_some() {
            Some(NotePlace::Reroll)
        } else if self.panel_open {
            Some(NotePlace::Panel)
        } else {
            None
        }
    }

    /// The note `place` shows, if it is on screen: the oldest waiting
    /// there, and how many more wait behind it.
    pub fn note_at(&self, place: NotePlace) -> Option<(Topic, usize)> {
        if self.note_place() != Some(place) {
            return None;
        }
        let mut waiting = self.notes.iter().filter(|(p, _)| *p == place);
        let (_, first) = waiting.next()?;
        Some((*first, waiting.count()))
    }

    /// The note's text as shown, `more` waiting behind it.
    pub fn note_text(topic: Topic, more: usize) -> String {
        let more = if more > 0 {
            format!(" (+{more} more)")
        } else {
            String::new()
        };
        format!("ⓘ {}{more}", topic.line())
    }

    /// Dismiss the note on screen; `false` when none is shown.
    fn dismiss_note(&mut self) -> bool {
        let Some(place) = self.note_place() else {
            return false;
        };
        match self.notes.iter().position(|(p, _)| *p == place) {
            Some(i) => {
                self.notes.remove(i);
                true
            }
            None => false,
        }
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
        if let Ok(mut s) = commands::status(&self.view, self.since_call) {
            s.cost_usd += self.cost_carried;
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
            Event::Switched { path, outcome } => self.switched(path, outcome),
            Event::RerollStarted(head) => {
                let screen = self.reroll.get_or_insert_with(RerollScreen::new);
                screen.head = Some(head);
            }
            Event::RerollProgress(v) => {
                if let Some(screen) = self.reroll.as_mut() {
                    screen.variants.push(v);
                }
                self.refresh_status();
            }
            Event::RerollDone(outcome) => {
                let topics = match (&outcome, &self.reroll) {
                    (Ok(()), Some(r)) => explain::reroll_topics(&reroll::summarize(&r.variants)),
                    _ => Vec::new(),
                };
                self.explain_first(NotePlace::Reroll, topics);
                if let Some(screen) = self.reroll.as_mut() {
                    screen.running = false;
                    if let Err(e) = outcome {
                        if screen.head.is_none() {
                            // Nothing ran: say why where the player is.
                            self.reroll = None;
                            self.reroll_open = false;
                            self.flash = Some(e);
                        } else {
                            screen.error = Some(e);
                        }
                    }
                }
                self.refresh_status();
            }
            Event::Adopted { variant, outcome } => match outcome {
                Ok(path) => {
                    self.reroll_open = false;
                    // Its variants belong to the save being left.
                    self.reroll = None;
                    let name = path.file_name().map_or_else(
                        || path.display().to_string(),
                        |n| n.to_string_lossy().into_owned(),
                    );
                    self.after_switch = Some(format!("Adopted variant {variant} as {name}."));
                    self.switch_to(path);
                }
                Err(e) => self.flash = Some(e),
            },
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
        if let Some(x) = &top.xray {
            let seq = match (&x.turn, x.follow) {
                (Some(t), false) => Some(t.seq),
                _ => xray::latest_turn(&self.view).ok().flatten(),
            };
            self.show_turn(seq);
            return;
        }
        if top.browser.is_some() {
            let target = top
                .browser
                .as_ref()
                .and_then(|p| p.links.iter().find(|l| Some(l.line) == top.selected))
                .map(|l| l.arg.clone());
            let result = commands::browser_query(&self.view, top.mode, &top.cmd, &top.arg);
            if let Some(top) = self.panel.last_mut() {
                match result {
                    Ok(page) => {
                        top.selected = target
                            .and_then(|arg| page.links.iter().find(|l| l.arg == arg))
                            .or(page.links.first())
                            .map(|l| l.line);
                        top.text = page.text.clone();
                        top.browser = Some(page);
                    }
                    Err(e) => {
                        top.text = format!("[compendium unavailable: {e}]");
                        top.selected = None;
                        top.browser = Some(browser::Page::default());
                    }
                }
                top.scroll = top.scroll.min(
                    crate::wrap::wrap(&top.text, self.panel_width.max(1))
                        .lines()
                        .count()
                        .saturating_sub(1),
                );
            }
            return;
        }
        if let Some(Ok(text)) = commands::report_in(&self.view, top.mode, &top.cmd, &top.arg)
            && let Some(top) = self.panel.last_mut()
        {
            top.text = text;
        }
    }

    /// Push a view onto the panel and show it.
    pub fn open_view(&mut self, cmd: &str, arg: &str, text: String) {
        let browser = commands::is_moded(cmd)
            .then(|| commands::browser_query(&self.view, self.mode, cmd, arg))
            .transpose();
        let browser = match browser {
            Ok(page) => page,
            Err(e) => {
                self.flash = Some(format!("[compendium unavailable: {e}]"));
                return;
            }
        };
        let view = PanelView {
            cmd: cmd.to_string(),
            arg: browser
                .as_ref()
                .and_then(|p| p.key.clone())
                .unwrap_or_else(|| arg.to_string()),
            text: browser.as_ref().map_or(text, |p| p.text.clone()),
            scroll: 0,
            selected: browser
                .as_ref()
                .and_then(|p| p.links.first())
                .map(|l| l.line),
            browser,
            xray: None,
            mode: self.mode,
        };
        // The same report again replaces itself rather than stacking.
        if self
            .panel
            .last()
            .is_some_and(|v| v.cmd == view.cmd && v.arg == view.arg && v.mode == view.mode)
        {
            self.panel.pop();
        }
        self.panel.push(view);
        self.panel_open = true;
    }

    /// Open (or redraw) the `:brain` table with `row` selected.
    fn show_brains(&mut self, row: usize) {
        self.open_view(":brain", "", self.brains.table());
        self.explain_first(NotePlace::Panel, [Topic::Brain]);
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

    /// Ctrl-X or `:xray`: open the X-ray panel on the latest turn, or close
    /// it when it is the view on top.
    pub fn toggle_xray(&mut self) {
        if self.xray_on_top() {
            self.panel_back();
        } else {
            self.open_xray(None);
        }
    }

    fn xray_on_top(&self) -> bool {
        self.panel_open && self.panel.last().is_some_and(|v| v.xray.is_some())
    }

    /// Open the X-ray panel on turn `seq`, or following the latest turn.
    pub fn open_xray(&mut self, seq: Option<i64>) {
        self.open_view(":xray", "", String::new());
        if let Some(top) = self.panel.last_mut() {
            top.xray = Some(XrayPanel {
                follow: seq.is_none(),
                turn: None,
                selected: 0,
            });
        }
        let seq = seq.or_else(|| xray::latest_turn(&self.view).ok().flatten());
        self.show_turn(seq);
    }

    /// Show turn `seq` in the X-ray panel on top, keeping the selection
    /// when it is the same turn.
    fn show_turn(&mut self, seq: Option<i64>) {
        let turn = match seq {
            Some(seq) => match xray::turn(&self.view, seq) {
                Ok(t) => t,
                Err(e) => {
                    self.flash = Some(format!("[:xray unavailable: {e}]"));
                    return;
                }
            },
            None => None,
        };
        let Some(top) = self.panel.last_mut() else {
            return;
        };
        let Some(x) = top.xray.as_mut() else {
            return;
        };
        let same = x.turn.as_ref().map(|t| t.seq) == turn.as_ref().map(|t| t.seq);
        if !same {
            x.selected = 0;
            top.scroll = 0;
        }
        let calls = turn.as_ref().map_or(0, |t| t.calls.len());
        x.selected = x.selected.min(calls.saturating_sub(1));
        let topics = turn.as_ref().map(explain::turn_topics).unwrap_or_default();
        x.turn = turn;
        self.explain_first(NotePlace::Panel, topics);
    }

    /// Keys for the X-ray panel; `false` leaves the key to the panel.
    fn xray_key(&mut self, key: KeyEvent) -> bool {
        let Some(x) = self.panel.last().and_then(|v| v.xray.as_ref()) else {
            return false;
        };
        let seq = x.turn.as_ref().map(|t| t.seq);
        let calls = x.turn.as_ref().map_or(0, |t| t.calls.len());
        let selected = x.selected;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.select_call(selected.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => {
                self.select_call((selected + 1).min(calls.saturating_sub(1)))
            }
            KeyCode::Enter | KeyCode::Char('c') => match seq {
                Some(seq) if calls > 0 => self.open_context(seq, selected + 1),
                _ => self.flash = Some("This turn has no recorded calls.".into()),
            },
            KeyCode::Char(c @ ('[' | ']')) => {
                let Some(seq) = seq else {
                    return true;
                };
                match xray::step_turn(&self.view, seq, c == '[') {
                    Ok(Some(next)) => {
                        let latest = xray::latest_turn(&self.view).ok().flatten();
                        if let Some(x) = self.panel.last_mut().and_then(|v| v.xray.as_mut()) {
                            x.follow = Some(next) == latest;
                        }
                        self.show_turn(Some(next));
                    }
                    Ok(None) if c == '[' => self.flash = Some("This is the first turn.".into()),
                    Ok(None) => {
                        self.flash =
                            Some("This is the latest turn; X-ray follows new ones.".into());
                        if let Some(x) = self.panel.last_mut().and_then(|v| v.xray.as_mut()) {
                            x.follow = true;
                        }
                    }
                    Err(e) => self.flash = Some(format!("[:xray unavailable: {e}]")),
                }
            }
            KeyCode::Char('r') => match seq {
                Some(seq) => self.command(&format!(":reroll #{seq}")),
                None => self.flash = Some("No turn to reroll yet.".into()),
            },
            KeyCode::Char('s') => match seq {
                Some(seq) => self.command(&format!(":surgery #{seq}")),
                None => self.flash = Some("No turn to operate on yet.".into()),
            },
            _ => return false,
        }
        true
    }

    /// Select call `i` in the X-ray panel and scroll it into view.
    fn select_call(&mut self, i: usize) {
        if let Some(x) = self.panel.last_mut().and_then(|v| v.xray.as_mut()) {
            x.selected = i;
        }
        let rows = self.panel_rows(self.panel_width);
        let Some(first) = rows.iter().position(|(_, sel)| *sel) else {
            return;
        };
        let last = rows.iter().rposition(|(_, sel)| *sel).unwrap_or(first);
        let height = self.panel_height.max(1);
        if let Some(v) = self.panel.last_mut() {
            if first < v.scroll {
                v.scroll = first;
            } else if last >= v.scroll + height {
                v.scroll = (last + 1).saturating_sub(height).min(first);
            }
        }
    }

    /// Open the context viewer on call `n` of turn `seq`.
    pub fn open_context(&mut self, seq: i64, n: usize) {
        match xray::turn_call(&self.view, seq, n) {
            Ok(Some(call)) => self.view_call(seq, n, &call),
            Ok(None) => self.flash = Some(format!("Turn #{seq} has no call {n}.")),
            Err(e) => self.flash = Some(format!("[:context unavailable: {e}]")),
        }
    }

    fn view_call(&mut self, seq: i64, n: usize, call: &StoredCall) {
        match xray::call_tree(&self.view, call) {
            Ok((tree, banner)) => {
                self.context = Some(ContextView {
                    title: commands::call_title(seq, n, call),
                    banner,
                    tree,
                    cursor: 0,
                    scroll: 0,
                    raw: xray::tree::raw_json(call),
                    typing: None,
                    query: None,
                });
                self.explain_first(NotePlace::Context, [Topic::Context]);
            }
            Err(e) => self.flash = Some(format!("[:context unavailable: {e}]")),
        }
    }

    /// Keys while the context viewer is open.
    fn context_key(&mut self, key: KeyEvent) {
        let typing = self.context.as_ref().is_some_and(|cv| cv.typing.is_some());
        if !typing && key.code == KeyCode::Char('x') && self.dismiss_note() {
            return;
        }
        let height = self.context_height.max(1);
        let Some(cv) = self.context.as_mut() else {
            return;
        };
        if let Some(q) = cv.typing.as_mut() {
            match key.code {
                KeyCode::Esc => cv.typing = None,
                KeyCode::Backspace => {
                    q.pop();
                }
                KeyCode::Char(c) => q.push(c),
                KeyCode::Enter => {
                    let q = cv.typing.take().unwrap_or_default();
                    if !q.is_empty() {
                        cv.query = Some(q);
                        self.context_search(true);
                    }
                }
                _ => {}
            }
            return;
        }
        let rows = cv.tree.rows().len();
        let last = rows.saturating_sub(1);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.context = None,
            KeyCode::Up | KeyCode::Char('k') => cv.cursor = cv.cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => cv.cursor = (cv.cursor + 1).min(last),
            KeyCode::PageUp => cv.cursor = cv.cursor.saturating_sub(height),
            KeyCode::PageDown => cv.cursor = (cv.cursor + height).min(last),
            KeyCode::Char('g') | KeyCode::Home => cv.cursor = 0,
            KeyCode::Char('G') | KeyCode::End => cv.cursor = last,
            KeyCode::Enter | KeyCode::Char(' ') => {
                if let Some(row) = cv.tree.rows().get(cv.cursor).copied() {
                    cv.tree.toggle(row.node);
                    // Folding from a body line lands on the node's label.
                    if let Some(at) = cv
                        .tree
                        .rows()
                        .iter()
                        .position(|r| r.node == row.node && r.line.is_none())
                    {
                        cv.cursor = at;
                    }
                }
            }
            KeyCode::Char('/') => cv.typing = Some(String::new()),
            KeyCode::Char('n') => self.context_search(true),
            KeyCode::Char('N') => self.context_search(false),
            KeyCode::Char('y') => {
                self.action = Some(TermAction::Copy(cv.raw.clone()));
                self.flash = Some("Copied the call's JSON to the clipboard (OSC 52).".into());
            }
            KeyCode::Char('e') => self.action = Some(TermAction::Edit(cv.raw.clone())),
            KeyCode::Char('?') => self.help_open = true,
            _ => {}
        }
    }

    /// Move the viewer's cursor to the next (or previous) node matching the
    /// last search, opening what hides it.
    fn context_search(&mut self, forward: bool) {
        let Some(cv) = self.context.as_mut() else {
            return;
        };
        let Some(query) = cv.query.clone() else {
            return;
        };
        let hits = cv.tree.search(&query);
        let here = cv.tree.rows().get(cv.cursor).map_or(0, |r| r.node);
        let found = if forward {
            hits.iter().find(|&&n| n > here).or(hits.first())
        } else {
            hits.iter().rev().find(|&&n| n < here).or(hits.last())
        };
        let Some(&node) = found else {
            self.flash = Some(format!("No match for {query}."));
            return;
        };
        cv.tree.reveal(node, &query);
        if let Some(at) = cv
            .tree
            .rows()
            .iter()
            .position(|r| r.node == node && r.line.is_none())
        {
            cv.cursor = at;
        }
    }

    /// Open (or redraw) the `:branches` tree with `row` selected, or the
    /// current save's row.
    fn show_branches(&mut self, row: Option<usize>) {
        let text = match commands::branches(&self.view) {
            Ok(text) => text,
            Err(e) => {
                self.flash = Some(format!("[:branches unavailable: {e}]"));
                return;
            }
        };
        self.branches = self
            .view
            .path()
            .and_then(|p| crate::branches::scan(&p).ok())
            .unwrap_or_default();
        let row = row
            .or_else(|| self.branches.iter().position(|b| b.current))
            .unwrap_or(0)
            .min(self.branches.len().saturating_sub(1));
        self.open_view(":branches", "", text);
        self.explain_first(NotePlace::Panel, [Topic::Branch]);
        if let Some(v) = self.panel.last_mut()
            && !self.branches.is_empty()
        {
            v.selected = Some(row);
        }
    }

    /// The selected branch's row, while the `:branches` view is on top.
    fn branch_row(&self) -> Option<usize> {
        let top = self.panel.last().filter(|v| v.cmd == ":branches")?;
        top.selected.filter(|&r| r < self.branches.len())
    }

    /// Keys in the `:branches` view; `false` when the key isn't one of them.
    fn branch_key(&mut self, key: KeyEvent) -> bool {
        let Some(row) = self.branch_row() else {
            return false;
        };
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.show_branches(Some(row.saturating_sub(1))),
            KeyCode::Down | KeyCode::Char('j') => self.show_branches(Some(row + 1)),
            KeyCode::Enter => self.command(&format!(":switch {}", row + 1)),
            KeyCode::Char('f') => {
                if self.refuse_while_busy() {
                    return true;
                }
                // Forks the latest event: the game as it stands.
                let at = self.view.last_seq().unwrap_or(0);
                self.flash = Some(match commands::fork(&self.view, &at.to_string()) {
                    Ok(said) | Err(said) => said,
                });
                self.show_branches(Some(row));
            }
            _ => return false,
        }
        true
    }

    /// `:reroll [#turn] [n] [knobs…]` starts a reroll and opens its view;
    /// `:reroll show <n>` reopens the latest one on variant `n`.
    fn reroll_command(&mut self, arg: &str) {
        if let Some(rest) = arg.strip_prefix("show") {
            let Some(screen) = self.reroll.as_mut() else {
                self.flash = Some("No reroll this session; :reroll runs one.".into());
                return;
            };
            match rest.trim().parse::<usize>() {
                Ok(n) if (1..=screen.n()).contains(&n) => {
                    screen.selected = n;
                    screen.mode = RerollMode::Narration;
                    screen.scroll = 0;
                    self.reroll_open = true;
                }
                Ok(_) | Err(_) if rest.trim().is_empty() => self.reroll_open = true,
                _ => self.flash = Some(format!("Usage: :reroll show <1-{}>", screen.n())),
            }
            return;
        }
        if self.refuse_while_busy() {
            return;
        }
        match reroll::Spec::parse(arg) {
            Ok(spec) => {
                self.reroll = Some(RerollScreen::new());
                self.reroll_open = true;
                self.send(Command::Reroll(spec));
            }
            Err(e) => self.flash = Some(e),
        }
    }

    /// `:surgery [#turn] [system] [n] [knobs…]` (or `s` in the X-ray):
    /// build the director's prompt for the turn and ask the event loop to
    /// open it in `$EDITOR`.
    fn surgery_command(&mut self, arg: &str) {
        if self.refuse_while_busy() {
            return;
        }
        let prepared = surgery::Spec::parse(arg)
            .and_then(|spec| surgery::prepare(&self.view, &spec).map(|prepared| (spec, prepared)));
        match prepared {
            Ok((spec, prepared)) => self.action = Some(TermAction::Surgery(spec, prepared)),
            Err(e) => self.flash = Some(e),
        }
    }

    /// The editor closed on a `:surgery` prompt: `edited` is the text it
    /// left, or `None` when it failed. An empty or unchanged edit cancels
    /// without calling a model; otherwise the surgery runs into the reroll
    /// view.
    pub fn surgery_edited(
        &mut self,
        spec: surgery::Spec,
        prepared: surgery::Prepared,
        edited: Option<String>,
    ) {
        let Some(edited) = edited else {
            self.flash = Some(format!(
                "{} exited with an error; surgery cancelled.",
                super::editor_command()
            ));
            return;
        };
        let Some(edit) = surgery::apply(prepared, &edited) else {
            self.flash = Some("Nothing edited: surgery cancelled, no model called.".into());
            return;
        };
        if self.refuse_while_busy() {
            return;
        }
        self.reroll = Some(RerollScreen::new());
        self.reroll_open = true;
        self.send(Command::Surgery(surgery::Surgery { spec, edit }));
    }

    /// `a` or `:adopt <n>`: make variant `n` of the latest reroll a new
    /// branch, then switch to it.
    fn adopt(&mut self, n: usize) {
        if self.reroll.as_ref().is_some_and(|r| r.running) {
            self.flash = Some("The reroll is still running; wait for it.".into());
            return;
        }
        if self.refuse_while_busy() {
            return;
        }
        self.send(Command::Adopt(n));
    }

    /// Keys while the reroll view is open.
    fn reroll_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Char('x') && self.dismiss_note() {
            return;
        }
        let height = self.story_height.max(1);
        let Some(screen) = self.reroll.as_mut() else {
            self.reroll_open = false;
            return;
        };
        let n = screen.n().max(1);
        match (screen.mode, key.code) {
            (RerollMode::Columns, KeyCode::Esc | KeyCode::Char('q')) => self.reroll_open = false,
            (_, KeyCode::Esc | KeyCode::Char('q')) => {
                screen.mode = RerollMode::Columns;
                screen.scroll = 0;
            }
            (_, KeyCode::Left | KeyCode::Char('h')) => {
                screen.selected = screen.selected.saturating_sub(1).max(1);
                screen.scroll = 0;
            }
            (_, KeyCode::Right | KeyCode::Char('l')) => {
                screen.selected = (screen.selected + 1).min(n);
                screen.scroll = 0;
            }
            (_, KeyCode::Char(c @ '1'..='8')) => {
                let pick = c as usize - '0' as usize;
                if pick <= n {
                    screen.selected = pick;
                    screen.scroll = 0;
                }
            }
            (_, KeyCode::Enter) => {
                screen.mode = RerollMode::Narration;
                screen.scroll = 0;
            }
            (_, KeyCode::Char('d')) => {
                screen.mode = RerollMode::Diff;
                screen.scroll = 0;
            }
            (_, KeyCode::Down | KeyCode::Char('j')) => screen.scroll += 1,
            (_, KeyCode::Up | KeyCode::Char('k')) => {
                screen.scroll = screen.scroll.saturating_sub(1)
            }
            (_, KeyCode::PageDown | KeyCode::Char(' ')) => screen.scroll += height,
            (_, KeyCode::PageUp) => screen.scroll = screen.scroll.saturating_sub(height),
            (_, KeyCode::Char('g')) => screen.scroll = 0,
            (_, KeyCode::Char('c')) => {
                let selected = screen.selected;
                self.open_variant_context(selected);
            }
            (_, KeyCode::Char('a')) => {
                let selected = screen.selected;
                self.adopt(selected);
            }
            (_, KeyCode::Char('?')) => self.help_open = true,
            _ => {}
        }
    }

    /// Open the context viewer on variant `n`'s director call (or its
    /// first call, on the fast path).
    fn open_variant_context(&mut self, n: usize) {
        let Some(id) = self
            .reroll
            .as_ref()
            .and_then(|r| r.head.as_ref())
            .map(|h| h.id)
        else {
            return;
        };
        let calls = match self.view.variant_calls(id, n as i64) {
            Ok(c) => c,
            Err(e) => {
                self.flash = Some(format!("[:context unavailable: {e}]"));
                return;
            }
        };
        let Some(at) = calls
            .iter()
            .position(|c| c.call.role == "director")
            .or((!calls.is_empty()).then_some(0))
        else {
            self.flash = Some(format!("Variant {n} has no recorded calls (yet)."));
            return;
        };
        self.view_variant_call(id, n, at + 1, &calls[at]);
    }

    fn view_variant_call(&mut self, reroll: i64, n: usize, k: usize, call: &StoredCall) {
        let seq = call.turn_seq.unwrap_or(0);
        self.view_call(seq, k, call);
        if let Some(cv) = self.context.as_mut() {
            cv.title = commands::variant_call_title(reroll, n, k, call);
        }
    }

    /// Says so and returns `true` while the worker is busy.
    fn refuse_while_busy(&mut self) -> bool {
        if self.busy() {
            self.flash = Some("The world is still answering; wait for it.".into());
        }
        self.busy()
    }

    /// `:switch`: point the view at the other save, then ask the worker to
    /// play it. The old view closes first, so the worker's writer is the
    /// last connection to the old save and checkpoints it.
    fn switch_to(&mut self, path: PathBuf) {
        let view = match World::open_read_only(&path) {
            Ok(v) => v,
            Err(e) => {
                self.flash = Some(format!("[can't open {}: {e}]", path.display()));
                return;
            }
        };
        self.switching_from = self.view.path();
        self.cost_carried += self.view.cost_since(self.since_call).unwrap_or(0.0);
        self.since_call = view.last_call_id().unwrap_or(0);
        self.view = view;
        // Another save starts over in Player mode.
        self.mode = Mode::Player;
        // Even Player entries can mean something different in another save.
        self.clear_compendium_views();
        self.refresh_panel();
        self.send(Command::Switch(path));
    }

    /// The worker finished a switch: reload the story from the branch, or
    /// go back to the old save's view if it failed.
    fn switched(&mut self, path: PathBuf, outcome: Result<String, String>) {
        let from = self.switching_from.take();
        match outcome {
            Ok(look) => {
                // Another save: its own notes (a new branch keeps its
                // parent's).
                self.explained = Notes::load(&self.view);
                self.explain_first(NotePlace::Panel, [Topic::Branch]);
                self.story.clear();
                self.streaming = false;
                self.story_scroll = 0;
                self.load_recent(RESUME_STORY);
                let name = path.file_name().map_or_else(
                    || path.display().to_string(),
                    |n| n.to_string_lossy().into_owned(),
                );
                if let Some(said) = self.after_switch.take() {
                    self.info(said);
                }
                self.info(format!("Switched to {name}.\n\n{look}"));
            }
            Err(e) => {
                self.after_switch = None;
                if let Some(from) = from
                    && let Ok(view) = World::open_read_only(&from)
                {
                    self.since_call = view.last_call_id().unwrap_or(0);
                    self.view = view;
                }
                self.notice(format!("[switch failed: {e}]"));
            }
        }
        self.refresh_status();
        // Turn numbers mean something else in another branch: X-ray views
        // go back to following the latest turn.
        for x in self.panel.iter_mut().filter_map(|v| v.xray.as_mut()) {
            x.follow = true;
        }
        if self.panel.last().is_some_and(|v| v.cmd == ":branches") {
            self.show_branches(None);
        } else {
            self.refresh_panel();
        }
    }

    /// Change the shared mode. Entry ids mean different things in the two
    /// projections, so reset browser history before any row can be activated.
    /// Unrelated workbench views keep their place in the back stack.
    fn set_mode(&mut self, mode: Mode, reopen: bool) {
        let reopen_cmd = if self.panel.last().is_some_and(|v| v.cmd == ":quests") {
            ":quests"
        } else {
            ":compendium"
        };
        let moded_on_top = self.panel_open
            && self
                .panel
                .last()
                .is_some_and(|v| commands::is_moded(&v.cmd));
        if self.mode != mode {
            self.clear_compendium_views();
        }
        self.mode = mode;
        if reopen && moded_on_top {
            if let Some(Ok(text)) = commands::report_in(&self.view, mode, reopen_cmd, "") {
                self.open_view(reopen_cmd, "", text);
            }
            return;
        }
        self.refresh_panel();
    }

    fn clear_compendium_views(&mut self) {
        self.panel.retain(|v| !commands::is_moded(&v.cmd));
        self.clear_panel_search();
        if self.panel.is_empty() {
            self.panel_open = false;
            if self.focus == Focus::Panel {
                self.focus = Focus::Input;
            }
        }
    }

    fn clear_panel_search(&mut self) {
        if self
            .last_search
            .as_ref()
            .is_some_and(|(f, _)| *f == Focus::Panel)
        {
            self.last_search = None;
        }
        if self.search.as_ref().is_some_and(|s| s.pane == Focus::Panel) {
            self.search = None;
        }
    }

    /// Esc or `h` in the panel: back one view; past the first, close it.
    fn panel_back(&mut self) {
        self.panel.pop();
        self.refresh_panel();
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
        match cmd {
            ":branches" => {
                self.show_branches(None);
                self.focus = Focus::Panel;
                return;
            }
            ":xray" => {
                match arg {
                    "on" if !self.xray_on_top() => self.open_xray(None),
                    "off" if self.xray_on_top() => self.panel_back(),
                    "on" | "off" => {}
                    "" => self.toggle_xray(),
                    _ => self.flash = Some("Usage: :xray [on|off]".into()),
                }
                return;
            }
            ":context" => {
                match commands::context(&self.view, arg) {
                    Ok(ContextReport::Text(text)) => {
                        self.open_view(cmd, arg, text);
                        self.explain_first(NotePlace::Panel, [Topic::Context]);
                    }
                    Ok(ContextReport::Call { seq, n, call, .. }) => self.view_call(seq, n, &call),
                    Ok(ContextReport::VariantCall {
                        reroll,
                        variant,
                        k,
                        call,
                        ..
                    }) => self.view_variant_call(reroll, variant, k, &call),
                    Err(notice) => self.flash = Some(notice),
                }
                return;
            }
            ":mode" => {
                match commands::mode_switch(arg, self.mode) {
                    Ok(mode) => {
                        self.set_mode(mode, true);
                        self.flash = Some(commands::mode_line(mode));
                    }
                    Err(usage) => self.flash = Some(usage.into()),
                }
                return;
            }
            _ => {}
        }
        // A mode named in a mode-aware command becomes the session's, so
        // the panel never holds a World view in Player mode.
        let (mode, arg) = match commands::resolve_mode(&self.view, self.mode, cmd, arg) {
            Ok(resolved) => resolved,
            Err(notice) => {
                self.flash = Some(notice);
                return;
            }
        };
        let arg = arg.as_str();
        if mode != self.mode {
            self.set_mode(mode, false);
            self.flash = Some(commands::mode_line(mode));
        }
        if let Some(report) = commands::report_in(&self.view, self.mode, cmd, arg) {
            match report {
                Ok(text) => {
                    self.open_view(cmd, arg, text);
                    let topics = commands::command_topics(cmd).iter().copied();
                    self.explain_first(NotePlace::Panel, topics);
                }
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
            ":switch" => {
                if self.refuse_while_busy() {
                    return;
                }
                match commands::switch_target(&self.view, arg) {
                    Ok(path) => self.switch_to(path),
                    Err(said) => self.flash = Some(said),
                }
            }
            ":reroll" => self.reroll_command(arg),
            ":surgery" => self.surgery_command(arg),
            ":adopt" => match arg.trim().parse::<usize>() {
                Ok(n) => self.adopt(n),
                Err(_) => {
                    self.flash = Some("Usage: :adopt <n> (a variant of the latest reroll)".into())
                }
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
            KeyCode::Char('n') if ctrl => {
                if !self.dismiss_note() {
                    self.flash = Some("No note to dismiss.".into());
                }
                return;
            }
            _ if self.context.is_some() && !self.help_open => {
                self.context_key(key);
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
            KeyCode::Char('x') if ctrl => {
                self.toggle_xray();
                return;
            }
            KeyCode::Char('b') if ctrl => {
                self.command(":branches");
                return;
            }
            KeyCode::Char('r') if ctrl => {
                self.command(":reroll");
                return;
            }
            _ if self.reroll_open && !self.help_open => {
                self.reroll_key(key);
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
            let last = KEYS.lines().count().saturating_sub(1);
            match key.code {
                KeyCode::Esc | KeyCode::Char('?' | 'q') => {
                    self.help_open = false;
                    self.help_scroll = 0;
                }
                KeyCode::Down | KeyCode::Char('j') => self.help_scroll += 1,
                KeyCode::Up | KeyCode::Char('k') => {
                    self.help_scroll = self.help_scroll.saturating_sub(1)
                }
                KeyCode::Char(' ') => self.help_scroll += 10,
                KeyCode::Char('g') => self.help_scroll = 0,
                KeyCode::Char('G') => self.help_scroll = last,
                _ => {}
            }
            self.help_scroll = self.help_scroll.min(last);
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

    /// The compendium keeps the panel's focus/back/scroll conventions.
    fn browser_key(&mut self, key: KeyEvent) -> bool {
        let Some(top) = self.panel.last().filter(|v| v.browser.is_some()) else {
            return false;
        };
        let page = top.browser.as_ref().unwrap();
        let at = page
            .links
            .iter()
            .position(|l| Some(l.line) == top.selected)
            .unwrap_or(0);
        match key.code {
            KeyCode::Char('m') => {
                self.set_mode(self.mode.toggled(), true);
                self.focus = Focus::Panel;
            }
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                if let Some(link) = page.links.get(at) {
                    let arg = link.arg.clone();
                    let cmd = link.cmd;
                    self.open_view(cmd, &arg, String::new());
                }
            }
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Down | KeyCode::Char('j')
                if !page.links.is_empty() =>
            {
                let next = if matches!(key.code, KeyCode::Up | KeyCode::Char('k')) {
                    at.saturating_sub(1)
                } else {
                    (at + 1).min(page.links.len() - 1)
                };
                let line = page.links[next].line;
                self.panel.last_mut().unwrap().selected = Some(line);
                let rows = self.panel_rows(self.panel_width);
                if let Some(first) = rows.iter().position(|(_, selected)| *selected) {
                    let last = rows
                        .iter()
                        .rposition(|(_, selected)| *selected)
                        .unwrap_or(first);
                    let height = self.panel_height.max(1);
                    let top = self.panel.last_mut().unwrap();
                    if first < top.scroll {
                        top.scroll = first;
                    } else if last >= top.scroll + height {
                        top.scroll = (last + 1).saturating_sub(height).min(first);
                    }
                }
            }
            _ => return false,
        }
        true
    }

    fn panel_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Char('x') && self.dismiss_note() {
            return;
        }
        if self.brain_key(key)
            || self.xray_key(key)
            || self.branch_key(key)
            || self.browser_key(key)
        {
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
                    if pane == Focus::Panel
                        && self.panel.last().is_some_and(|v| v.browser.is_some())
                    {
                        let cmd = if self.panel.last().is_some_and(|v| v.cmd == ":quests") {
                            ":quests"
                        } else {
                            ":compendium"
                        };
                        self.open_view(cmd, &format!("search {query}"), String::new());
                        return;
                    }
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
        self.panel_rows(width).into_iter().map(|(l, _)| l).collect()
    }

    /// The top panel view wrapped at `width`, each line marked when it
    /// belongs to the X-ray's selected call.
    pub fn panel_rows(&self, width: usize) -> Vec<(String, bool)> {
        let wrap = |text: &str| -> Vec<String> {
            crate::wrap::wrap(text, width.max(1))
                .lines()
                .map(str::to_string)
                .collect()
        };
        match self.panel.last() {
            Some(PanelView { xray: Some(x), .. }) => match &x.turn {
                None => wrap(NO_TURN_YET).into_iter().map(|l| (l, false)).collect(),
                Some(turn) => xray::panel_lines(turn)
                    .into_iter()
                    .flat_map(|(line, call)| {
                        let selected = call == Some(x.selected);
                        let lines = if line.is_empty() {
                            vec![line]
                        } else {
                            wrap(&line)
                        };
                        lines.into_iter().map(move |l| (l, selected))
                    })
                    .collect(),
            },
            // A selected text line stays highlighted whole when it wraps.
            Some(v) => v
                .text
                .split('\n')
                .enumerate()
                .flat_map(|(i, line)| {
                    let lines = if line.is_empty() {
                        vec![String::new()]
                    } else {
                        wrap(line)
                    };
                    lines.into_iter().map(move |l| (l, v.selected == Some(i)))
                })
                .collect(),
            None => vec![(EMPTY_PANEL.to_string(), false)],
        }
    }

    /// The mouse: the wheel scrolls the pane under it, a click focuses it.
    pub fn on_mouse(&mut self, m: MouseEvent) {
        // The context viewer covers everything: the wheel moves its cursor.
        if let Some(cv) = self.context.as_mut() {
            let last = cv.tree.rows().len().saturating_sub(1);
            match m.kind {
                MouseEventKind::ScrollUp => cv.cursor = cv.cursor.saturating_sub(3),
                MouseEventKind::ScrollDown => cv.cursor = (cv.cursor + 3).min(last),
                _ => {}
            }
            return;
        }
        // So does the reroll view: the wheel scrolls it.
        if self.reroll_open
            && let Some(r) = self.reroll.as_mut()
        {
            match m.kind {
                MouseEventKind::ScrollUp => r.scroll = r.scroll.saturating_sub(3),
                MouseEventKind::ScrollDown => r.scroll += 3,
                _ => {}
            }
            return;
        }
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
        let hints = self.focus_hints();
        match self.note_place().and_then(|p| self.note_at(p)) {
            Some(_) if self.search.is_none() => format!("Ctrl-N dismiss note · {hints}"),
            _ => hints,
        }
    }

    fn focus_hints(&self) -> String {
        if let Some(search) = &self.search {
            return format!("/{} · Enter find · Esc cancel", search.query);
        }
        if self.help_open {
            return "j/k scroll · Space page · g/G top/bottom · Esc close help".into();
        }
        if let Some(cv) = &self.context {
            return match &cv.typing {
                Some(q) => format!("/{q}  · Enter find · Esc cancel"),
                None => "↑↓ j/k move · Enter/Space fold · / search · n/N next · \
                         y copy JSON · e $EDITOR · Esc close"
                    .into(),
            };
        }
        if self.reroll_open
            && let Some(r) = &self.reroll
        {
            let adopt = if r.running { "" } else { " · a adopt" };
            return match r.mode {
                RerollMode::Columns => format!(
                    "←→ variant · Enter narration · d diff · c context{adopt} · Esc close · ? help"
                ),
                _ => format!(
                    "←→ variant · j/k scroll · Enter narration · d diff · c context{adopt} · \
                     Esc back"
                ),
            };
        }
        if self.focus == Focus::Panel && self.xray_on_top() {
            return "↑↓ select call · Enter/c context · [ ] turn · r reroll · \
                    s surgery · Esc back · Tab focus"
                .into();
        }
        match self.focus {
            Focus::Input => "Enter send · ↑↓ history · :cmd Tab completes · Tab focus · \
                             PgUp/PgDn scroll · Ctrl-P panel · Ctrl-L log · Ctrl-C quit"
                .into(),
            Focus::Story => "j/k scroll · g/G top/bottom · / search · n/N next · Tab focus · \
                             Esc input · ? help"
                .into(),
            Focus::Panel if self.panel.last().is_some_and(|v| v.browser.is_some()) => {
                "↑↓ select · Enter open · / search · m Player/World · Space scroll · Esc/h back · Tab focus".into()
            }
            Focus::Panel if self.brain_row().is_some() => {
                "↑↓ role · Enter model · e effort · Esc/h back · Tab focus · ? help".into()
            }
            Focus::Panel if self.branch_row().is_some() => {
                "↑↓ branch · Enter switch · f fork here · Esc/h back · Tab focus · ? help".into()
            }
            Focus::Panel => "j/k scroll · g/G top/bottom · / search · n/N next · Esc/h back · \
                             Tab focus · ? help"
                .into(),
        }
    }
}

/// Story paragraphs a resumed save, or a branch switched to, opens with.
pub const RESUME_STORY: usize = 20;

/// The input prompt; it sits in the story's margin.
pub const PROMPT: &str = "› ";
/// Story text's left margin, in columns.
pub const MARGIN: usize = crate::wrap::DEFAULT_MARGIN;

/// What the X-ray says before any turn is played.
pub const NO_TURN_YET: &str = "No turns played yet. The X-ray shows each turn's model calls, \
cost, cache use, refusals, rules and rolls once you play one.";

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
        "{}\n\nTUI: Ctrl-X (or :xray) toggles the X-ray panel; in it ↑↓ select a call, \
         Enter opens its context, [ ] step turns, r rerolls the turn and s opens its director \
         prompt in $EDITOR for :surgery. Ctrl-R (or :reroll) rerolls the last turn into a \
         full-screen view of variants; a surgery's edited variants are marked ✂. Ctrl-N \
         dismisses a first-time note. :panel toggles this panel (Ctrl-P), :mouse [on|off] \
         switches mouse capture (off restores native text selection; Shift-drag selects while \
         it's on). \
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

:compendium panel
  ↑ ↓  j k          select category, entry, related entry or source
  Enter → l         open selection
  /                 search the compendium
  m                 toggle Player / World (resets to categories)
  Space  g G        page, top, bottom of the account
  Esc h ←           back

:brain panel
  ↑ ↓  j k          pick a role
  Enter             next model (haiku → sonnet → opus)
  e                 next effort (low → medium → high → xhigh → max)

:branches panel (Ctrl-B)
  ↑ ↓  j k          pick a branch
  Enter             switch to it
  f                 fork the game as it stands into a new branch

X-ray panel (Ctrl-X)
  ↑ ↓ / Enter c     select a call / open its context
  [ ]   r s         previous, next turn   reroll this turn, edit it (surgery)

Reroll view (Ctrl-R, :reroll, :surgery; ✂ marks edited variants)
  ← → h l  1-8      pick a variant
  Enter   d         its whole narration, its full diff
  c                 its director call in the context viewer
  a                 adopt it: a new branch, switched to
  j k  Esc          scroll   back to the columns, then close

Context viewer
  Enter Space  / n N    fold, unfold   search, next, previous
  y   e   Esc q         copy JSON (OSC 52)   open it in $EDITOR   close

Anywhere
  PgUp PgDn         scroll the story
  Ctrl-P            toggle the side panel
  Ctrl-L            event log
  Ctrl-X            X-ray panel
  Ctrl-B            branches
  Ctrl-R            reroll the last turn
  Ctrl-N            dismiss the note at the top of the panel or view
                    (x too, while it has focus); :explain repeats any
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
        seen_notes(App::new(game.into_world(), true))
    }

    /// `app` with every first-time note already shown, so tests about
    /// something else don't meet one.
    pub(crate) fn seen_notes(mut app: App) -> App {
        app.explained = Notes::all();
        app
    }

    /// `app` as a new save sees it: no notes shown yet.
    pub(crate) fn fresh_notes(mut app: App) -> App {
        app.explained = Notes::default();
        app
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

    /// Every title and text the panel holds, back stack included.
    fn panel_texts(app: &App) -> Vec<String> {
        app.panel
            .iter()
            .flat_map(|v| [v.title(), v.text.clone()])
            .collect()
    }

    /// What only World mode shows in the begun game.
    const WORLD_ONLY: [&str; 5] = ["Brother Ash", "Eel-wives", "Saltmarsh", "altar", "bible"];

    #[test]
    fn the_panel_shows_the_session_mode_and_later_views_share_it() {
        let mut app = app();
        assert_eq!(app.mode, Mode::Player, "a session starts in Player mode");
        typed(&mut app, ":compendium");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.panel.last().unwrap().title(), ":compendium · Player");

        typed(&mut app, ":mode");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.mode, Mode::World);
        assert!(app.flash.as_deref().unwrap().contains("World"));
        let top = app.panel.last().unwrap();
        assert_eq!(top.title(), ":compendium · World");
        let world = browser::query(&app.view, Mode::World, "").unwrap().text;
        assert_eq!(top.text, world);

        typed(&mut app, ":compendium people");
        app.on_key(key(KeyCode::Enter));
        let top = app.panel.last().unwrap();
        assert_eq!(top.title(), ":compendium people · World");
        assert!(
            top.text.contains("Brother Ash (person · bible plan)"),
            "{}",
            top.text
        );
        // A refresh reads it again in its own mode.
        app.refresh_panel();
        assert!(app.panel.last().unwrap().text.contains("Brother Ash"));
        // Other reports don't carry a mode.
        typed(&mut app, ":time");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.panel.last().unwrap().title(), ":time");
        assert!(
            app.outbox.is_empty(),
            "toggling and reading send nothing to the game"
        );
    }

    #[test]
    fn returning_to_player_mode_leaves_no_world_only_text_in_the_panel() {
        let mut app = app();
        for line in [
            ":compendium",
            ":time",
            ":compendium world",
            ":compendium brother ash",
        ] {
            typed(&mut app, line);
            app.on_key(key(KeyCode::Enter));
        }
        assert_eq!(
            app.mode,
            Mode::World,
            "a mode named in the command becomes the session's"
        );
        assert!(
            app.panel
                .last()
                .unwrap()
                .text
                .contains("Last monk, lies kindly.")
        );
        app.last_search = Some((Focus::Panel, "Brother Ash".into()));

        typed(&mut app, ":mode player");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.mode, Mode::Player);
        assert_eq!(app.last_search, None);
        assert!(
            app.panel
                .iter()
                .all(|v| v.mode == Mode::Player || v.cmd == ":time")
        );
        for text in panel_texts(&app) {
            for hidden in WORLD_ONLY {
                assert!(
                    !text.contains(hidden),
                    "{hidden} left in the panel:\n{text}"
                );
            }
        }
        // Back through the history: only Player views and the clock.
        let titles: Vec<String> = app.panel.iter().map(PanelView::title).collect();
        assert_eq!(titles, [":time", ":compendium · Player"]);

        // Naming Player explicitly drops World views the same way.
        typed(&mut app, ":compendium world brother ash");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(
            app.panel.last().unwrap().title(),
            ":compendium bible.npcs.1 · World"
        );
        typed(&mut app, ":compendium player");
        app.on_key(key(KeyCode::Enter));
        for text in panel_texts(&app) {
            assert!(!text.contains("Brother Ash"), "{text}");
        }
        // A mode word that isn't followed by a section, search or key is
        // part of a name: it never stores a World report as a Player view.
        typed(&mut app, ":compendium player world");
        app.on_key(key(KeyCode::Enter));
        let top = app.panel.last().unwrap();
        assert_eq!(top.mode, Mode::Player);
        assert!(top.text.contains("No entries found"), "{}", top.text);
        for text in panel_texts(&app) {
            for hidden in WORLD_ONLY {
                assert!(!text.contains(hidden), "{hidden} in the panel:\n{text}");
            }
        }
        // With only World views, leaving World closes the panel.
        let mut app = self::app();
        typed(&mut app, ":compendium world");
        app.on_key(key(KeyCode::Enter));
        typed(&mut app, ":mode");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.panel.last().unwrap().title(), ":compendium · Player");
        assert_eq!(app.panel.len(), 1);
        assert!(app.outbox.is_empty());
    }

    #[test]
    fn a_switched_save_starts_in_player_mode() {
        use crate::branches::tests::{saved_game, temp_dir};
        let dir = temp_dir("tui-mode-switch");
        let main = saved_game(&dir, "mill.db");
        let other = dir.join("mill.b1.db");
        let world = World::open(&main).unwrap();
        drop(world.fork(world.last_seq().unwrap(), &other).unwrap());
        drop(world);
        let mut app = seen_notes(App::new(World::open_read_only(&main).unwrap(), true));
        for line in [":compendium", ":compendium world brother ash"] {
            typed(&mut app, line);
            app.on_key(key(KeyCode::Enter));
        }
        assert_eq!(app.mode, Mode::World);
        app.switch_to(other.clone());
        app.on_event(Event::Switched {
            path: other,
            outcome: Ok("Drowned Chapel.".into()),
        });
        app.on_event(Event::Ready);
        assert_eq!(app.mode, Mode::Player);
        assert!(app.panel.iter().all(|v| v.mode == Mode::Player));
        for text in panel_texts(&app) {
            assert!(!text.contains("Brother Ash"), "{text}");
        }
    }

    #[test]
    fn rules_and_glossary_open_learned_mechanics_in_the_panel_as_plain_mode_prints_them() {
        let mut app = app();
        for cmd in [":rules", ":glossary"] {
            let plain = crate::commands::report(&app.view, cmd, "")
                .unwrap()
                .unwrap();
            assert!(plain.contains("known yet"), "{plain}");
            typed(&mut app, cmd);
            app.on_key(key(KeyCode::Enter));
            assert!(app.panel.last().unwrap().text.contains("Mechanics"));
            assert!(app.panel.last().unwrap().text.contains("No entries found"));
        }
        assert!(
            app.outbox.is_empty(),
            "reading them sends nothing to the game"
        );
    }

    #[test]
    fn the_compendium_opens_in_the_panel_as_plain_mode_prints_it() {
        let mut app = app();
        let plain = |arg: &str| {
            crate::commands::report(&app.view, ":compendium", arg)
                .unwrap()
                .unwrap()
        };
        let (overview, chapel) = (plain(""), plain("drowned"));
        assert!(overview.contains("Drowned Chapel (place)"), "{overview}");
        typed(&mut app, ":compendium");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(
            app.panel
                .last()
                .unwrap()
                .browser
                .as_ref()
                .unwrap()
                .links
                .len(),
            6
        );
        typed(&mut app, ":compendium drowned");
        app.on_key(key(KeyCode::Enter));
        assert!(app.panel.last().unwrap().text.starts_with(&chapel));
        assert!(
            app.outbox.is_empty(),
            "reading it sends nothing to the game"
        );
    }

    #[test]
    fn ctrl_l_opens_the_log_and_reserved_keys_explain_themselves() {
        let mut app = app();
        app.on_key(ctrl('l'));
        assert_eq!(app.panel.last().unwrap().cmd, ":log");
        // Not a file save: the tree says there are no branches.
        app.on_key(ctrl('b'));
        assert!(app.panel.last().unwrap().text.contains("no branches"));
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
    fn the_branch_tree_switches_and_forks() {
        use crate::branches::tests::{saved_game, temp_dir};
        let dir = temp_dir("tui-branches");
        let main = saved_game(&dir, "mill.db");
        let world = World::open(&main).unwrap();
        let branch = dir.join("mill.b1.db");
        let at = world.last_seq().unwrap();
        let b = world.fork(at, &branch).unwrap();
        b.set_label("early").unwrap();
        drop(b);
        let mut app = seen_notes(App::new(World::open_read_only(&main).unwrap(), true));

        // Ctrl-B opens the tree on the current save, focused.
        app.on_key(ctrl('b'));
        assert_eq!(app.focus, Focus::Panel);
        let top = app.panel.last().unwrap();
        assert_eq!(top.cmd, ":branches");
        assert!(top.text.contains("* 1  mill.db"), "{}", top.text);
        let line = format!("└─ mill.b1.db “early” · from #{at}");
        assert!(top.text.contains(&line), "{}", top.text);
        assert_eq!(app.branch_row(), Some(0));
        assert!(app.hints().contains("Enter switch"));
        // Enter on the current save says so; ↓ Enter switches.
        app.on_key(key(KeyCode::Enter));
        assert!(app.flash.as_deref().unwrap().contains("already playing"));
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.branch_row(), Some(1));
        app.on_key(key(KeyCode::Enter));
        let canonical = std::fs::canonicalize(&branch).unwrap();
        assert_eq!(app.outbox, [Command::Switch(canonical.clone())]);
        assert!(app.busy());
        // While it switches, more switching and forking are refused.
        app.on_key(key(KeyCode::Char('f')));
        assert!(app.flash.as_deref().unwrap().contains("still answering"));

        app.info("left behind");
        app.on_event(Event::Switched {
            path: canonical.clone(),
            outcome: Ok("Drowned Chapel. Water everywhere.".into()),
        });
        app.on_event(Event::Ready);
        assert!(!app.story.iter().any(|e| e.text == "left behind"));
        assert!(app.story[0].text.contains("flooded chapel"));
        assert!(
            app.story
                .last()
                .unwrap()
                .text
                .starts_with("Switched to mill.b1.db.")
        );
        let top = app.panel.last().unwrap();
        assert!(top.text.contains("* 2  └─ mill.b1.db"), "{}", top.text);
        assert_eq!(app.branch_row(), Some(1));

        // f forks the branch as it stands into the next free name.
        app.outbox.clear();
        app.on_key(key(KeyCode::Char('f')));
        assert!(
            app.flash.as_deref().unwrap().contains("mill.b2.db"),
            "{:?}",
            app.flash
        );
        assert!(dir.join("mill.b2.db").exists());
        assert!(app.panel.last().unwrap().text.contains("mill.b2.db"));
        assert!(app.outbox.is_empty());

        // :switch by name goes back.
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Input);
        typed(&mut app, ":switch mill");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(
            app.outbox,
            [Command::Switch(std::fs::canonicalize(&main).unwrap())]
        );
        drop((app, world));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failed_switch_returns_to_the_old_save() {
        use crate::branches::tests::{saved_game, temp_dir};
        let dir = temp_dir("tui-switch-fail");
        let main = saved_game(&dir, "mill.db");
        let world = World::open(&main).unwrap();
        let branch = dir.join("mill.b1.db");
        drop(world.fork(2, &branch).unwrap());
        let mut app = seen_notes(App::new(World::open_read_only(&main).unwrap(), true));
        typed(&mut app, ":switch 2");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(
            app.view.path(),
            Some(std::fs::canonicalize(&branch).unwrap())
        );
        app.on_event(Event::Switched {
            path: branch.clone(),
            outcome: Err("locked".into()),
        });
        assert_eq!(app.view.path(), Some(std::fs::canonicalize(&main).unwrap()));
        assert!(app.story.last().unwrap().text.contains("switch failed"));
        drop((app, world));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_wrapped_selected_row_is_one_source_line() {
        let mut app = app();
        app.open_view(":brain", "", "head\na long row that wraps\nlast".into());
        app.panel.last_mut().unwrap().selected = Some(1);
        let rows = app.panel_rows(8);
        let lit: Vec<&str> = rows
            .iter()
            .filter(|(_, s)| *s)
            .map(|(l, _)| l.as_str())
            .collect();
        assert!(lit.len() > 1, "{rows:?}");
        assert_eq!(lit.join(" "), "a long row that wraps");
        assert_eq!(rows.last().unwrap(), &("last".to_string(), false));
    }

    #[test]
    fn resuming_loads_recent_story() {
        let mut app = app();
        app.load_recent(10);
        assert_eq!(app.story[0].text, "You wake in a flooded chapel.");
    }

    /// An app over a save with two played turns (the first from
    /// [`crate::xray::tests::played`]); returns their input events.
    pub(crate) fn xray_app() -> (App, i64, i64) {
        use crate::llm::{Response, ScriptedClient};
        let (mut game, first) = crate::xray::tests::played();
        let client = ScriptedClient::new([
            Response::canned_tool_use(
                "i",
                "interpret",
                serde_json::json!({"matches": [], "clean": false}),
            ),
            Response::canned_text("Nothing stirs."),
        ]);
        game.turn(&client, "wait", &mut |_| {}).unwrap();
        let second = crate::xray::latest_turn(game.world()).unwrap().unwrap();
        (seen_notes(App::new(game.into_world(), true)), first, second)
    }

    /// A reroll's head and a finished variant `n`, as the worker sends them.
    pub(crate) fn reroll_events(n: usize) -> (Head, Vec<VariantView>) {
        use crate::reroll::{Fact, Played};
        let head = Head {
            id: 1,
            turn_seq: 9,
            input: "light the candle".into(),
            n,
            knobs: serde_json::json!({}),
            dice: "same".into(),
        };
        let variants = (1..=n)
            .map(|k| VariantView {
                n: k,
                mark: crate::reroll::Mark::Plain,
                result: Ok(Played {
                    narration: format!("Narration of variant {k}."),
                    route: "director".into(),
                    facts: if k == 1 {
                        vec![Fact {
                            key: "candle.lit: false → true".into(),
                            text: "candle.lit: false → true".into(),
                            engine: false,
                        }]
                    } else {
                        vec![]
                    },
                }),
                calls: 1,
                cost_usd: 0.001,
            })
            .collect();
        (head, variants)
    }

    #[test]
    fn ctrl_r_starts_a_reroll_whose_view_fills_in_and_adopts() {
        let mut app = app();
        app.on_key(ctrl('r'));
        assert_eq!(app.outbox, [Command::Reroll(reroll::Spec::default())]);
        assert!(app.reroll_open);
        // A second one waits for the first.
        app.on_key(ctrl('r'));
        assert_eq!(app.outbox.len(), 1);
        let (head, variants) = reroll_events(3);
        app.on_event(Event::RerollStarted(head));
        app.on_event(Event::RerollProgress(variants[0].clone()));
        // Adopting waits for the whole reroll.
        app.on_key(key(KeyCode::Char('a')));
        assert!(app.flash.as_deref().unwrap().contains("still running"));
        for v in &variants[1..] {
            app.on_event(Event::RerollProgress(v.clone()));
        }
        app.on_event(Event::RerollDone(Ok(())));
        app.on_event(Event::Ready);
        let r = app.reroll.as_ref().unwrap();
        assert_eq!((r.variants.len(), r.running), (3, false));
        app.flash = None;
        assert!(app.hints().contains("a adopt"));

        app.on_key(key(KeyCode::Right));
        app.on_key(key(KeyCode::Right));
        app.on_key(key(KeyCode::Right));
        assert_eq!(app.reroll.as_ref().unwrap().selected, 3);
        app.on_key(key(KeyCode::Left));
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.reroll.as_ref().unwrap().mode, RerollMode::Narration);
        app.on_key(key(KeyCode::Char('d')));
        assert_eq!(app.reroll.as_ref().unwrap().mode, RerollMode::Diff);
        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.reroll.as_ref().unwrap().mode, RerollMode::Columns);
        app.on_key(key(KeyCode::Char('a')));
        assert_eq!(app.outbox.last(), Some(&Command::Adopt(2)));
        app.on_event(Event::Adopted {
            variant: 2,
            outcome: Err("This game isn't saved in a file.".into()),
        });
        app.on_event(Event::Ready);
        assert!(app.flash.as_deref().unwrap().contains("isn't saved"));
        app.on_key(key(KeyCode::Esc));
        assert!(!app.reroll_open);

        // `:reroll show` reopens it on a variant; `:adopt` adopts by number.
        typed(&mut app, ":reroll show 3");
        app.on_key(key(KeyCode::Enter));
        let r = app.reroll.as_ref().unwrap();
        assert!(app.reroll_open);
        assert_eq!((r.selected, r.mode), (3, RerollMode::Narration));
        app.reroll_open = false;
        typed(&mut app, ":adopt 1");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.outbox.last(), Some(&Command::Adopt(1)));
    }

    #[test]
    fn a_reroll_that_cannot_start_says_why() {
        let mut app = app();
        typed(&mut app, ":reroll 12");
        app.on_key(key(KeyCode::Enter));
        assert!(app.flash.as_deref().unwrap().contains("1 to 8"));
        assert!(app.outbox.is_empty());
        typed(&mut app, ":reroll director=haiku 2");
        app.on_key(key(KeyCode::Enter));
        app.on_event(Event::RerollDone(Err("No turn to reroll yet.".into())));
        assert!(!app.reroll_open);
        assert_eq!(app.flash.as_deref(), Some("No turn to reroll yet."));
    }

    #[test]
    fn r_in_the_xray_rerolls_the_turn_shown() {
        let (mut app, first, _) = xray_app();
        app.on_key(ctrl('x'));
        app.focus = Focus::Panel;
        app.on_key(key(KeyCode::Char('[')));
        app.on_key(key(KeyCode::Char('r')));
        assert_eq!(
            app.outbox.last(),
            Some(&Command::Reroll(reroll::Spec {
                turn: Some(first),
                ..reroll::Spec::default()
            }))
        );
    }

    #[test]
    fn c_in_the_reroll_view_opens_a_variants_director_call() {
        use crate::llm::{Response, ScriptedClient};
        let (game, _) = crate::xray::tests::played();
        let scripts = [ScriptedClient::new([Response::canned_text("Again.")])];
        let mut started = None;
        let mut done = Vec::new();
        reroll::run(
            game.world(),
            &reroll::Spec {
                n: 1,
                ..reroll::Spec::default()
            },
            game.brains(),
            &|n| &scripts[n - 1] as &(dyn crate::llm::LlmClient + Sync),
            &mut |p| match p {
                reroll::Progress::Started(h) => started = Some(h.clone()),
                reroll::Progress::Variant(v) => done.push(v.clone()),
            },
        )
        .unwrap();
        let mut app = seen_notes(App::new(game.into_world(), true));
        app.on_key(ctrl('r'));
        app.on_event(Event::RerollStarted(started.unwrap()));
        app.on_event(Event::RerollProgress(done.remove(0)));
        app.on_event(Event::RerollDone(Ok(())));
        app.on_key(key(KeyCode::Char('c')));
        let cv = app.context.as_ref().expect("context viewer");
        assert!(
            cv.title.starts_with("Reroll r1 variant 1 call"),
            "{}",
            cv.title
        );
        assert!(cv.title.contains("director"), "{}", cv.title);
        // Esc closes the viewer back onto the reroll view.
        app.on_key(key(KeyCode::Esc));
        assert!(app.context.is_none() && app.reroll_open);
    }

    fn xray_turn(app: &App) -> Option<i64> {
        app.panel
            .last()
            .and_then(|v| v.xray.as_ref())
            .and_then(|x| x.turn.as_ref())
            .map(|t| t.seq)
    }

    #[test]
    fn xray_notes_show_once_per_save_and_ctrl_n_or_x_dismisses_them() {
        let (app, first, _) = xray_app();
        let mut app = fresh_notes(app);
        // Off, the X-ray explains nothing.
        assert!(app.notes.is_empty());
        app.on_key(ctrl('x'));
        // The latest turn went to the director.
        assert_eq!(app.note_at(NotePlace::Panel).unwrap().0, Topic::Director);
        assert_eq!(
            app.outbox,
            [Command::Explained(vec![Topic::Director])],
            "recorded through the worker"
        );
        assert!(app.hints().starts_with("Ctrl-N dismiss note"));
        // Stepping back to the first turn adds what it shows.
        app.on_key(key(KeyCode::Tab));
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Panel);
        app.on_key(key(KeyCode::Char('[')));
        assert_eq!(xray_turn(&app), Some(first));
        let queued: Vec<Topic> = app.notes.iter().map(|(_, t)| *t).collect();
        assert_eq!(
            queued,
            [
                Topic::Director,
                Topic::Cache,
                Topic::Cost,
                Topic::Refusal,
                Topic::Rule
            ]
        );
        assert_eq!(app.note_at(NotePlace::Panel), Some((Topic::Director, 4)));
        app.on_key(ctrl('n'));
        assert_eq!(app.note_at(NotePlace::Panel), Some((Topic::Cache, 3)));
        // `x` dismisses too while the panel has focus.
        app.on_key(key(KeyCode::Char('x')));
        assert_eq!(app.note_at(NotePlace::Panel).unwrap().0, Topic::Cost);
        // Nothing new turns up for topics already shown.
        app.outbox.clear();
        app.on_key(key(KeyCode::Char(']')));
        app.on_key(key(KeyCode::Char('[')));
        assert!(app.outbox.is_empty());
        app.notes.clear();
        app.on_key(ctrl('n'));
        assert_eq!(app.flash.as_deref(), Some("No note to dismiss."));
    }

    #[test]
    fn views_explain_themselves_where_they_open() {
        let (app, first, _) = xray_app();
        let mut app = fresh_notes(app);
        app.open_context(first, 1);
        assert_eq!(app.note_at(NotePlace::Context), Some((Topic::Context, 0)));
        // The viewer covers the panel, whose notes wait.
        assert_eq!(app.note_at(NotePlace::Panel), None);
        app.on_key(key(KeyCode::Char('x')));
        assert!(app.notes.is_empty());
        app.on_key(key(KeyCode::Esc));
        typed(&mut app, ":context");
        app.on_key(key(KeyCode::Enter));
        assert!(app.notes.is_empty(), "context was explained already");
        typed(&mut app, ":brain");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.note_at(NotePlace::Panel), Some((Topic::Brain, 0)));
        app.notes.clear();
        app.focus = Focus::Input;
        typed(&mut app, ":fork 1");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.note_at(NotePlace::Panel), Some((Topic::Branch, 0)));

        // A reroll whose variants disagree explains the split.
        let mut app = fresh_notes(seen_notes(app));
        app.notes.clear();
        app.on_key(ctrl('r'));
        let (head, variants) = reroll_events(2);
        app.on_event(Event::RerollStarted(head));
        for v in variants {
            app.on_event(Event::RerollProgress(v));
        }
        assert!(app.notes.is_empty(), "nothing until it finishes");
        app.on_event(Event::RerollDone(Ok(())));
        assert_eq!(
            app.note_at(NotePlace::Reroll),
            Some((Topic::RerollSplit, 1))
        );
        assert!(
            App::note_text(Topic::RerollSplit, 1).starts_with("ⓘ reroll-split: The same moment")
                && App::note_text(Topic::RerollSplit, 1).ends_with("(+1 more)")
        );
    }

    #[test]
    fn explain_opens_in_the_panel() {
        let mut app = app();
        typed(&mut app, ":explain");
        app.on_key(key(KeyCode::Enter));
        let top = app.panel.last().unwrap();
        assert!(app.panel_open && top.text.starts_with("Topics"));
        typed(&mut app, ":explain refusal");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.panel.last().unwrap().text, Topic::Refusal.line());
        assert_eq!(app.panel.last().unwrap().title(), ":explain refusal");
    }

    #[test]
    fn ctrl_x_toggles_the_xray_on_the_latest_turn() {
        let (mut app, _, second) = xray_app();
        app.on_key(ctrl('x'));
        assert!(app.panel_open);
        assert_eq!(xray_turn(&app), Some(second));
        let x = app.panel.last().unwrap().xray.as_ref().unwrap();
        assert!(x.follow);
        assert_eq!(x.turn.as_ref().unwrap().calls[0].role, "interpret");
        app.on_key(ctrl('x'));
        assert!(!app.panel_open);
        typed(&mut app, ":xray");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(xray_turn(&app), Some(second));
        typed(&mut app, ":xray on");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.panel.len(), 1);
        typed(&mut app, ":xray off");
        app.on_key(key(KeyCode::Enter));
        assert!(!app.panel_open);
    }

    #[test]
    fn brackets_step_turns_and_the_latest_follows_again() {
        let (mut app, first, second) = xray_app();
        app.on_key(ctrl('x'));
        app.focus = Focus::Panel;
        app.on_key(key(KeyCode::Char('[')));
        assert_eq!(xray_turn(&app), Some(first));
        assert!(!app.panel.last().unwrap().xray.as_ref().unwrap().follow);
        app.on_key(key(KeyCode::Char('[')));
        assert!(app.flash.as_deref().unwrap().contains("first turn"));
        app.on_key(key(KeyCode::Char(']')));
        assert_eq!(xray_turn(&app), Some(second));
        assert!(app.panel.last().unwrap().xray.as_ref().unwrap().follow);
        app.on_key(key(KeyCode::Char(']')));
        assert!(app.flash.as_deref().unwrap().contains("latest turn"));
    }

    #[test]
    fn arrows_select_a_call_and_enter_opens_its_context() {
        let (mut app, first, _) = xray_app();
        app.on_key(ctrl('x'));
        app.focus = Focus::Panel;
        app.on_key(key(KeyCode::Char('[')));
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Char('j')));
        app.on_key(key(KeyCode::Up));
        assert_eq!(app.panel.last().unwrap().xray.as_ref().unwrap().selected, 1);
        for _ in 0..10 {
            app.on_key(key(KeyCode::Down));
        }
        assert_eq!(app.panel.last().unwrap().xray.as_ref().unwrap().selected, 4);
        app.on_key(key(KeyCode::Char('k')));
        app.on_key(key(KeyCode::Enter));
        let cv = app.context.as_ref().unwrap();
        assert_eq!(
            cv.title,
            format!("Turn #{first} call 4 · director · sonnet")
        );
        assert!(app.hints().contains("Enter/Space fold"));
        app.on_key(key(KeyCode::Esc));
        assert!(app.context.is_none());
        app.on_key(key(KeyCode::Char('c')));
        assert!(app.context.is_some());
    }

    #[test]
    fn s_in_the_xray_opens_the_turns_prompt_for_surgery() {
        let (mut app, first, _) = xray_app();
        app.on_key(ctrl('x'));
        app.focus = Focus::Panel;
        app.on_key(key(KeyCode::Char('[')));
        app.on_key(key(KeyCode::Char('s')));
        let Some(TermAction::Surgery(spec, prepared)) = app.action.take() else {
            panic!("expected a surgery action, got {:?}", app.action);
        };
        assert_eq!(prepared.turn_seq, first);
        assert_eq!(spec.target, surgery::Target::Message);
        assert!(prepared.text().contains("Player input:"));
        assert!(app.outbox.is_empty(), "nothing runs before the edit");

        // An unchanged edit cancels with no worker command.
        let text = prepared.text().to_owned();
        app.surgery_edited(spec.clone(), prepared.clone(), Some(format!("{text}\n")));
        assert!(app.flash.as_deref().unwrap().contains("surgery cancelled"));
        assert!(app.outbox.is_empty() && !app.reroll_open);
        // So does an editor that failed.
        app.surgery_edited(spec.clone(), prepared.clone(), None);
        assert!(app.flash.as_deref().unwrap().contains("surgery cancelled"));
        assert!(app.outbox.is_empty());

        // A real edit runs into the reroll view.
        app.surgery_edited(
            spec,
            prepared,
            Some(text.replace("Player input:", "Hint: no.\nPlayer input:")),
        );
        assert!(app.reroll_open);
        match app.outbox.as_slice() {
            [Command::Surgery(s)] => {
                assert_eq!(s.edit.prepared.turn_seq, first);
                assert!(s.edit.edited.message.contains("Hint: no."));
            }
            other => panic!("expected a surgery command, got {other:?}"),
        }
    }

    #[test]
    fn surgery_reports_what_it_cannot_operate_on() {
        let (mut app, _, _) = xray_app();
        typed(&mut app, ":surgery #1");
        app.on_key(key(KeyCode::Enter));
        assert!(
            app.flash
                .as_deref()
                .unwrap()
                .contains("isn't a turn's input")
        );
        typed(&mut app, ":surgery 9");
        app.on_key(key(KeyCode::Enter));
        assert!(app.flash.as_deref().unwrap().contains("1 to 7"));
        assert!(app.action.is_none() && app.outbox.is_empty());
    }

    #[test]
    fn the_context_viewer_folds_searches_copies_and_edits() {
        let (mut app, first, _) = xray_app();
        app.open_context(first, 5);
        let rows = |app: &App| app.context.as_ref().unwrap().tree.rows().len();
        let before = rows(&app);
        // Move to "Turn message" and fold it.
        let at = {
            let cv = app.context.as_ref().unwrap();
            cv.tree
                .rows()
                .iter()
                .position(|r| cv.tree.nodes[r.node].label == "Turn message")
                .unwrap()
        };
        for _ in 0..at {
            app.on_key(key(KeyCode::Down));
        }
        app.on_key(key(KeyCode::Enter));
        assert!(rows(&app) < before);
        app.on_key(key(KeyCode::Char(' ')));
        assert_eq!(rows(&app), before);
        // Search opens the folded section holding the match.
        app.on_key(key(KeyCode::Char('/')));
        typed(&mut app, "1000 gold");
        assert!(app.hints().starts_with("/1000 gold"));
        app.on_key(key(KeyCode::Enter));
        let cv = app.context.as_ref().unwrap();
        let row = cv.tree.rows()[cv.cursor];
        assert_eq!(cv.tree.nodes[row.node].label, "Player input");
        assert!(!cv.tree.nodes[row.node].folded);
        app.on_key(key(KeyCode::Char('/')));
        typed(&mut app, "no such words");
        app.on_key(key(KeyCode::Enter));
        assert!(app.flash.as_deref().unwrap().starts_with("No match"));
        app.on_key(key(KeyCode::Char('y')));
        match &app.action {
            Some(TermAction::Copy(json)) => assert!(json.contains("\"role\": \"director\"")),
            other => panic!("expected a copy, got {other:?}"),
        }
        app.on_key(key(KeyCode::Char('e')));
        assert!(matches!(app.action, Some(TermAction::Edit(_))));
        // Keys don't leak to the input line underneath.
        typed(&mut app, "zz");
        assert!(app.input.is_empty());
        app.on_key(key(KeyCode::Char('q')));
        assert!(app.context.is_none());
    }

    #[test]
    fn the_wheel_moves_the_viewer_not_the_story_beneath() {
        use crossterm::event::MouseEvent;
        let (mut app, first, _) = xray_app();
        app.open_context(first, 5);
        let wheel = |kind| MouseEvent {
            kind,
            column: 5,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };
        app.on_mouse(wheel(MouseEventKind::ScrollDown));
        assert_eq!(app.context.as_ref().unwrap().cursor, 3);
        app.on_mouse(wheel(MouseEventKind::ScrollUp));
        assert_eq!(app.context.as_ref().unwrap().cursor, 0);
        assert_eq!(app.story_scroll, 0);
    }

    #[test]
    fn context_commands_open_the_list_or_the_viewer() {
        let (mut app, first, _) = xray_app();
        typed(&mut app, &format!(":context #{first}"));
        app.on_key(key(KeyCode::Enter));
        let top = app.panel.last().unwrap();
        assert_eq!(top.cmd, ":context");
        assert!(top.text.contains(&format!("#{first}.5 director")));
        typed(&mut app, &format!(":context #{first}.2"));
        app.on_key(key(KeyCode::Enter));
        assert!(app.context.as_ref().unwrap().title.contains("call 2"));
    }

    #[test]
    fn help_lists_the_workbench() {
        let mut app = app();
        typed(&mut app, ":help");
        app.on_key(key(KeyCode::Enter));
        let text = &app.panel.last().unwrap().text;
        assert!(
            text.contains("Workbench") && text.contains("Ctrl-X"),
            "{text}"
        );
    }
}

#[cfg(test)]
#[path = "browser_tests.rs"]
mod browser_tests;
