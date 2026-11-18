//! Drawing the TUI: story, side panel, status bar, input line, hint bar,
//! and the help overlay.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use super::app::{
    App, Areas, ContextView, Focus, KEYS, Kind, NotePlace, PROMPT, RerollMode, RerollScreen,
};
use crate::reroll;
use crate::xray::tree::token_label;

/// Width of the side panel beside the story.
fn panel_width(total: u16) -> u16 {
    (total * 2 / 5).max(48).min(total.saturating_sub(40))
}

/// Draw the whole screen and remember where things went.
pub fn draw(frame: &mut Frame, app: &mut App) {
    let [body, status, input, hints] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    let (story_area, panel_area) = if !app.panel_open {
        (Some(body), None)
    } else if app.panel_overlays(body.width) {
        (None, Some(body))
    } else {
        let [s, p] = Layout::horizontal([
            Constraint::Min(1),
            Constraint::Length(panel_width(body.width)),
        ])
        .areas(body);
        (Some(s), Some(p))
    };
    app.areas = Areas {
        story: story_area,
        panel: panel_area,
        input,
    };
    // The story may be hidden behind the panel; focus can't stay there.
    if story_area.is_none() && app.focus == Focus::Story {
        app.focus = Focus::Panel;
    }

    if let Some(area) = story_area {
        draw_story(frame, app, area);
    }
    if let Some(area) = panel_area {
        draw_panel(frame, app, area);
    }
    draw_status(frame, app, status);
    draw_input(frame, app, input);
    frame.render_widget(
        Paragraph::new(app.hints()).style(Style::new().fg(Color::DarkGray)),
        hints,
    );
    if app.reroll_open && app.reroll.is_some() {
        draw_reroll(frame, app, frame.area());
    }
    if app.context.is_some() {
        draw_context(frame, app, frame.area());
    }
    if app.help_open {
        draw_help(frame, app, frame.area());
    }
}

/// The first-time note `place` shows, wrapped to `width` (empty when
/// there is none).
fn note_lines(app: &App, place: NotePlace, width: u16) -> Vec<Line<'static>> {
    let Some((topic, more)) = app.note_at(place) else {
        return Vec::new();
    };
    let style = Style::new()
        .fg(Color::Yellow)
        .add_modifier(Modifier::ITALIC);
    wrapped(
        &App::note_text(topic, more),
        usize::from(width).saturating_sub(1),
    )
    .into_iter()
    .map(|l| Line::styled(format!(" {l}"), style))
    .collect()
}

/// One screen line of the context viewer: the tree row it belongs to and
/// its spans.
type ContextLine = (usize, Line<'static>);

/// The context viewer's tree as screen lines at `width`: labels with their
/// token counts (green when cached, yellow when partly), body lines wrapped
/// under a rule.
fn context_lines(cv: &ContextView, width: usize) -> Vec<ContextLine> {
    let mut out = Vec::new();
    for (i, row) in cv.tree.rows().iter().enumerate() {
        let node = &cv.tree.nodes[row.node];
        let pad = "  ".repeat(node.depth);
        match row.line {
            None => {
                let mut spans = vec![Span::raw(format!(
                    "{pad}{}{}",
                    cv.tree.marker(row.node),
                    node.label
                ))];
                if node.tokens > 0 {
                    let shade = if node.cached == 0 {
                        Style::new().fg(Color::DarkGray)
                    } else if node.cached >= node.tokens {
                        Style::new().fg(Color::Green)
                    } else {
                        Style::new().fg(Color::Yellow)
                    };
                    spans.push(Span::styled(format!("  {}", token_label(node)), shade));
                }
                let mut line = Line::from(spans);
                if node.depth == 0 {
                    line = line.patch_style(Style::new().add_modifier(Modifier::BOLD));
                }
                out.push((i, line));
            }
            Some(l) => {
                let lead = format!("{pad}    │ ");
                let room = width.saturating_sub(lead.chars().count()).max(10);
                let text = &node.body[l];
                let wrapped = crate::wrap::wrap(text, room);
                let mut parts: Vec<&str> = wrapped.lines().collect();
                if parts.is_empty() {
                    parts.push("");
                }
                for part in parts {
                    out.push((
                        i,
                        Line::from(vec![
                            Span::styled(lead.clone(), Style::new().fg(Color::DarkGray)),
                            Span::raw(part.to_string()),
                        ]),
                    ));
                }
            }
        }
    }
    out
}

fn draw_context(frame: &mut Frame, app: &mut App, area: Rect) {
    frame.render_widget(Clear, area);
    let note = note_lines(app, NotePlace::Context, area.width);
    let Some(cv) = app.context.as_mut() else {
        return;
    };
    let banner = cv.banner.is_some() as u16;
    let [title, note_area, banner_area, body, hints] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(note.len() as u16),
        Constraint::Length(banner),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(area);
    frame.render_widget(
        Paragraph::new(format!(" context · {} — Esc closes", cv.title))
            .style(Style::new().bg(Color::DarkGray).fg(Color::White)),
        title,
    );
    frame.render_widget(Paragraph::new(note), note_area);
    if let Some(b) = &cv.banner {
        frame.render_widget(
            Paragraph::new(format!(" ! {b}"))
                .style(Style::new().fg(Color::Black).bg(Color::Yellow)),
            banner_area,
        );
    }
    let height = body.height as usize;
    let lines = context_lines(cv, body.width as usize);
    let rows = cv.tree.rows().len();
    cv.cursor = cv.cursor.min(rows.saturating_sub(1));
    // Keep the cursor's lines on screen.
    let first = lines.iter().position(|(r, _)| *r == cv.cursor).unwrap_or(0);
    let last = lines
        .iter()
        .rposition(|(r, _)| *r == cv.cursor)
        .unwrap_or(first);
    if first < cv.scroll {
        cv.scroll = first;
    } else if last >= cv.scroll + height {
        cv.scroll = (last + 1).saturating_sub(height).min(first);
    }
    let query = cv.query.as_ref().map(|q| q.to_lowercase());
    let cursor = cv.cursor;
    let shown: Vec<Line> = lines
        .into_iter()
        .skip(cv.scroll)
        .take(height)
        .map(|(r, line)| {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            if r == cursor {
                line.patch_style(Style::new().add_modifier(Modifier::REVERSED))
            } else if query
                .as_ref()
                .is_some_and(|q| text.to_lowercase().contains(q))
            {
                line.patch_style(Style::new().bg(Color::Yellow).fg(Color::Black))
            } else {
                line
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(shown), body);
    app.context_height = height;
    frame.render_widget(
        Paragraph::new(app.hints()).style(Style::new().fg(Color::DarkGray)),
        hints,
    );
}

/// Narrowest a reroll column may be; narrower, the variants become tabs.
pub const MIN_COLUMN: u16 = 30;

/// Whether `n` variants fit side by side in `width` columns.
pub fn reroll_columns(width: u16, n: usize) -> bool {
    n > 0 && usize::from(width) >= n * usize::from(MIN_COLUMN)
}

/// Lines of narration a column shows before its diff.
const EXCERPT_LINES: usize = 8;

/// `text` wrapped to `width` as lines.
fn wrapped(text: &str, width: usize) -> Vec<String> {
    crate::wrap::wrap(text, width.max(10))
        .lines()
        .map(str::to_owned)
        .collect()
}

/// The full-screen reroll view: overview on top, then the variants in
/// columns (or tabs), or one variant's narration or diff.
fn draw_reroll(frame: &mut Frame, app: &mut App, area: Rect) {
    frame.render_widget(Clear, area);
    let hint_text = app.hints();
    let note = note_lines(app, NotePlace::Reroll, area.width);
    let Some(r) = app.reroll.as_mut() else {
        return;
    };
    let width = area.width as usize;
    let top: Vec<Line> = match &r.head {
        None => vec![Line::from("Reroll starting…")],
        Some(head) => {
            let mut lines = reroll::overview(head, &r.variants);
            if let Some(e) = &r.error {
                lines.push(format!("failed: {e}"));
            }
            let mut out = Vec::new();
            for (i, line) in lines.iter().enumerate() {
                for part in wrapped(line, width.saturating_sub(2)) {
                    let mut l = Line::from(format!(" {part}"));
                    if i == 0 {
                        l = l.patch_style(Style::new().add_modifier(Modifier::BOLD));
                    }
                    out.push(l);
                }
            }
            out
        }
    };
    let top_height = (top.len() as u16).min(area.height / 3).max(1);
    let [title, note_area, overview, body, hints] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(note.len() as u16),
        Constraint::Length(top_height),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(area);
    let state = if r.running { "running" } else { "done" };
    let what = if r.head.as_ref().is_some_and(reroll::Head::is_surgery) {
        "surgery"
    } else {
        "reroll"
    };
    frame.render_widget(
        Paragraph::new(format!(" {what} · {state} — Esc closes"))
            .style(Style::new().bg(Color::DarkGray).fg(Color::White)),
        title,
    );
    frame.render_widget(Paragraph::new(note), note_area);
    frame.render_widget(Paragraph::new(top), overview);
    frame.render_widget(
        Paragraph::new(hint_text).style(Style::new().fg(Color::DarkGray)),
        hints,
    );
    let n = r.n();
    if n == 0 {
        return;
    }
    match r.mode {
        RerollMode::Columns if reroll_columns(body.width, n) => {
            let columns = Layout::horizontal(vec![Constraint::Ratio(1, n as u32); n]).split(body);
            for (i, col) in columns.iter().enumerate() {
                draw_variant_column(frame, r, i + 1, *col);
            }
        }
        RerollMode::Columns => {
            let [tabs, rest] =
                Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(body);
            let spans: Vec<Span> = (1..=n)
                .map(|k| {
                    let running = if r.variant(k).is_some() { "" } else { "…" };
                    let mark = r.head.as_ref().map_or("", |h| h.mark(k).suffix());
                    let label = format!(" {k}{mark}{running} ");
                    if k == r.selected {
                        Span::styled(label, Style::new().add_modifier(Modifier::REVERSED))
                    } else {
                        Span::raw(label)
                    }
                })
                .collect();
            frame.render_widget(Paragraph::new(Line::from(spans)), tabs);
            let selected = r.selected;
            draw_variant_column(frame, r, selected, rest);
        }
        RerollMode::Narration | RerollMode::Diff => {
            let selected = r.selected;
            let lines: Vec<String> = match r.variant(selected) {
                None => vec!["running…".into()],
                Some(v) => {
                    let text = match (r.mode, &v.result) {
                        (RerollMode::Narration, Ok(p)) => p.narration.clone(),
                        (RerollMode::Narration, Err(e)) => format!("failed: {e}"),
                        _ => reroll::diff_lines(v, true).join("\n"),
                    };
                    text.lines()
                        .flat_map(|l| {
                            let w = wrapped(l, usize::from(body.width).saturating_sub(4));
                            if w.is_empty() { vec![String::new()] } else { w }
                        })
                        .collect()
                }
            };
            let height = usize::from(body.height.saturating_sub(2)).max(1);
            r.scroll = r.scroll.min(lines.len().saturating_sub(height));
            let what = if r.mode == RerollMode::Diff {
                "diff"
            } else {
                "narration"
            };
            let heading = r
                .variant(selected)
                .map_or_else(|| format!("Variant {selected}"), reroll::variant_heading);
            frame.render_widget(
                Paragraph::new(
                    lines
                        .into_iter()
                        .skip(r.scroll)
                        .take(height)
                        .map(|l| Line::from(format!(" {l}")))
                        .collect::<Vec<_>>(),
                )
                .block(
                    Block::new()
                        .borders(Borders::ALL)
                        .border_style(Style::new().fg(Color::Cyan))
                        .title(format!(" {heading} · {what} ")),
                ),
                body,
            );
        }
    }
}

/// One variant in a column (or as the open tab): heading, narration
/// excerpt, then its compact diff.
fn draw_variant_column(frame: &mut Frame, r: &RerollScreen, k: usize, area: Rect) {
    let selected = k == r.selected;
    let inner = usize::from(area.width.saturating_sub(4));
    let (title, lines): (String, Vec<Line>) = match r.variant(k) {
        None => (
            format!(
                " Variant {k}{} ",
                r.head.as_ref().map_or("", |h| h.mark(k).suffix())
            ),
            vec![Line::from(" running…")],
        ),
        Some(v) => {
            let mut lines = Vec::new();
            if let Ok(p) = &v.result {
                let mut excerpt = wrapped(&p.narration, inner);
                if excerpt.len() > EXCERPT_LINES {
                    excerpt.truncate(EXCERPT_LINES);
                    excerpt.push("…".into());
                }
                lines.extend(excerpt.into_iter().map(|l| Line::from(format!(" {l}"))));
            }
            lines.push(Line::from(""));
            for fact in reroll::diff_lines(v, false) {
                for (i, part) in wrapped(&fact, inner.saturating_sub(2))
                    .into_iter()
                    .enumerate()
                {
                    let lead = if i == 0 { " • " } else { "   " };
                    lines.push(Line::styled(
                        format!("{lead}{part}"),
                        Style::new().fg(Color::Yellow),
                    ));
                }
            }
            (format!(" {} ", reroll::variant_heading(v)), lines)
        }
    };
    let border = if selected {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(Color::DarkGray)
    };
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::new()
                .borders(Borders::ALL)
                .border_style(border)
                .title(title),
        ),
        area,
    );
}

fn focused_style(app: &App, pane: Focus) -> Style {
    if app.focus == pane {
        Style::new().fg(Color::Cyan)
    } else {
        Style::new().fg(Color::DarkGray)
    }
}

fn search_query(app: &App, pane: Focus) -> Option<String> {
    app.last_search
        .as_ref()
        .filter(|(p, q)| *p == pane && !q.is_empty())
        .map(|(_, q)| q.to_lowercase())
}

fn draw_story(frame: &mut Frame, app: &mut App, area: Rect) {
    // A focus marker in the top-right corner keeps the text area clean.
    let width = area.width as usize;
    let height = area.height as usize;
    app.story_width = width;
    app.story_height = height;
    let lines = app.story_lines(width);
    let total = lines.len();
    // Clamp the scroll so `g` and over-scrolling stop at the top.
    app.story_scroll = app.story_scroll.min(total.saturating_sub(height));
    let top = app.story_top(total);
    let query = search_query(app, Focus::Story);
    let shown: Vec<Line> = lines
        .into_iter()
        .skip(top)
        .take(height)
        .map(|(text, kind)| {
            let hit = query
                .as_ref()
                .is_some_and(|q| text.to_lowercase().contains(q));
            let mut line = match kind {
                Some(Kind::Input) => match text.strip_prefix(PROMPT) {
                    Some(rest) => Line::from(vec![
                        Span::styled(
                            PROMPT,
                            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(rest.to_string(), Style::new().add_modifier(Modifier::BOLD)),
                    ]),
                    None => Line::styled(text, Style::new().add_modifier(Modifier::BOLD)),
                },
                Some(Kind::Notice) => Line::styled(text, Style::new().add_modifier(Modifier::DIM)),
                _ => Line::from(text),
            };
            if hit {
                line = line.patch_style(Style::new().bg(Color::Yellow).fg(Color::Black));
            }
            line
        })
        .collect();
    frame.render_widget(Paragraph::new(shown), area);
    if app.focus == Focus::Story || app.story_scroll > 0 {
        let mark = if app.story_scroll > 0 {
            format!(" ↑{} ", app.story_scroll)
        } else {
            " story ".to_string()
        };
        let w = mark.chars().count() as u16;
        if area.width > w {
            let r = Rect::new(area.right() - w, area.y, w, 1);
            frame.render_widget(
                Paragraph::new(mark).style(focused_style(app, Focus::Story)),
                r,
            );
        }
    }
}

fn draw_panel(frame: &mut Frame, app: &mut App, area: Rect) {
    frame.render_widget(Clear, area);
    let depth = app.panel.len();
    let title = match app.panel.last() {
        Some(v) if depth > 1 => format!(" {} ({depth}) ", v.title()),
        Some(v) => format!(" {} ", v.title()),
        None => " panel ".to_string(),
    };
    let block = Block::new()
        .borders(Borders::LEFT | Borders::TOP)
        .title(title)
        .border_style(focused_style(app, Focus::Panel));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let inner = Rect {
        x: inner.x + 1,
        width: inner.width.saturating_sub(1),
        ..inner
    };
    let note = note_lines(app, NotePlace::Panel, inner.width);
    let [note_area, inner] =
        Layout::vertical([Constraint::Length(note.len() as u16), Constraint::Min(1)]).areas(inner);
    frame.render_widget(Paragraph::new(note), note_area);
    app.panel_width = inner.width as usize;
    app.panel_height = inner.height as usize;
    let lines = app.panel_rows(inner.width as usize);
    let scroll = app
        .panel
        .last()
        .map_or(0, |v| v.scroll)
        .min(lines.len().saturating_sub(1));
    let query = search_query(app, Focus::Panel);
    let shown: Vec<Line> = lines
        .into_iter()
        .skip(scroll)
        .take(inner.height as usize)
        .map(|(text, selected)| {
            let hit = query
                .as_ref()
                .is_some_and(|q| text.to_lowercase().contains(q));
            if selected {
                Line::styled(
                    format!("{text:<width$}", width = inner.width as usize),
                    Style::new().add_modifier(Modifier::REVERSED),
                )
            } else if hit {
                Line::styled(text, Style::new().bg(Color::Yellow).fg(Color::Black))
            } else {
                Line::from(text)
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(shown), inner);
}

/// The status line: game time, room, director model, session cost, and
/// whether the world is busy.
pub fn status_text(app: &App) -> String {
    let s = &app.status;
    let mut parts = Vec::new();
    if let Some(when) = &s.when {
        parts.push(when.clone());
    }
    if let Some(room) = &s.room {
        parts.push(room.clone());
    }
    parts.push(format!("director {}", app.brains.director_model()));
    parts.push(format!("${:.3}", s.cost_usd));
    if app.busy() {
        parts.push("● the world is answering…".into());
    }
    if !app.mouse {
        parts.push("mouse off".into());
    }
    format!(" {}", parts.join(" │ "))
}

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    frame.render_widget(
        Paragraph::new(status_text(app)).style(Style::new().bg(Color::DarkGray).fg(Color::White)),
        area,
    );
}

fn draw_input(frame: &mut Frame, app: &App, area: Rect) {
    let (prompt, text, cursor) = match &app.search {
        Some(s) => ("/ ".to_string(), s.query.clone(), s.query.chars().count()),
        None => (PROMPT.to_string(), app.input.clone(), app.cursor),
    };
    let room = (area.width as usize).saturating_sub(prompt.chars().count() + 1);
    // Scroll a long line so the caret stays on screen.
    let start = cursor.saturating_sub(room);
    let visible: String = text.chars().skip(start).take(room).collect();
    let prompt_style = if app.focus == Focus::Input || app.search.is_some() {
        Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(Color::DarkGray)
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(prompt.clone(), prompt_style),
            Span::styled(visible, Style::new().add_modifier(Modifier::BOLD)),
        ])),
        area,
    );
    if (app.focus == Focus::Input || app.search.is_some()) && !app.help_open {
        let x = area.x + (prompt.chars().count() + cursor - start) as u16;
        frame.set_cursor_position(Position::new(x.min(area.right().saturating_sub(1)), area.y));
    }
}

fn draw_help(frame: &mut Frame, app: &mut App, area: Rect) {
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .title(" keys — j/k scroll · Esc closes ")
        .border_style(Style::new().fg(Color::Cyan));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    // Stop scrolling once the last line is on screen.
    let lines = KEYS.lines().count();
    app.help_scroll = app
        .help_scroll
        .min(lines.saturating_sub(inner.height as usize));
    frame.render_widget(
        Paragraph::new(KEYS).scroll((app.help_scroll as u16, 0)),
        inner,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::tests::{app, ctrl, key, typed};
    use crossterm::event::KeyCode;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render(app: &mut App, w: u16, h: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect()
    }

    fn screen(rows: &[String]) -> String {
        rows.join("\n")
    }

    #[test]
    fn layout_has_story_status_input_and_hints() {
        let mut a = app();
        a.load_recent(10);
        typed(&mut a, "look");
        let rows = render(&mut a, 100, 12);
        assert!(
            rows[0].starts_with("  You wake in a flooded chapel."),
            "{rows:?}"
        );
        assert!(rows[9].contains("Drowned Chapel"), "{}", rows[9]);
        assert!(rows[9].contains("Day 1"));
        assert!(rows[9].contains("director sonnet"));
        assert!(rows[9].contains("$0.000"));
        assert!(rows[10].starts_with("› look"));
        assert!(rows[11].contains("Enter send"));
    }

    #[test]
    fn a_wide_screen_shows_the_panel_beside_the_story() {
        let mut a = app();
        a.load_recent(10);
        typed(&mut a, ":quests");
        a.on_key(key(KeyCode::Enter));
        let rows = render(&mut a, 140, 12);
        assert!(rows[0].starts_with("  You wake"), "{}", rows[0]);
        assert!(rows[0].contains(":quests"), "{}", rows[0]);
        assert!(a.areas.story.is_some() && a.areas.panel.is_some());
    }

    #[test]
    fn the_brain_panel_shows_the_table_and_the_status_bar_the_director() {
        let mut a = app();
        typed(&mut a, ":brain");
        a.on_key(key(KeyCode::Enter));
        a.on_key(key(KeyCode::Enter)); // director: sonnet → opus
        let rows = render(&mut a, 140, 14);
        let text = screen(&rows);
        assert!(rows[0].contains(":brain"), "{text}");
        let director = rows.iter().find(|r| r.contains("director  ")).unwrap();
        assert!(director.contains("* director   opus    medium"), "{text}");
        assert!(text.contains("interpret  haiku   default"), "{text}");
        assert!(text.contains("tick       sonnet  medium"), "{text}");
        assert!(rows[11].contains("director opus"), "{}", rows[11]);
        assert!(rows[13].contains("e effort"), "{}", rows[13]);

        // The selected row is drawn reversed.
        let mut terminal = Terminal::new(TestBackend::new(140, 14)).unwrap();
        terminal.draw(|f| draw(f, &mut a)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let y = rows.iter().position(|r| r.contains("* director")).unwrap() as u16;
        let row = &rows[y as usize];
        let x = row[..row.find("* director").unwrap()].chars().count() as u16;
        assert!(buf[(x, y)].modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn a_narrow_screen_overlays_the_panel() {
        let mut a = app();
        a.load_recent(10);
        typed(&mut a, ":quests");
        a.on_key(key(KeyCode::Enter));
        let rows = render(&mut a, 119, 12);
        assert!(!screen(&rows).contains("You wake"), "{}", screen(&rows));
        assert!(rows[0].contains(":quests"));
        assert!(a.areas.story.is_none());
        // Tab skips the hidden story.
        a.on_key(key(KeyCode::Tab));
        assert_eq!(a.focus, Focus::Panel);
        a.on_key(key(KeyCode::Tab));
        assert_eq!(a.focus, Focus::Input);
        // Esc from the input closes the overlay.
        a.on_key(key(KeyCode::Esc));
        let rows = render(&mut a, 119, 12);
        assert!(screen(&rows).contains("You wake"));
    }

    #[test]
    fn the_hint_bar_follows_focus() {
        let mut a = app();
        a.on_key(ctrl('p'));
        let hint = |a: &mut App| render(a, 130, 8)[7].clone();
        assert!(hint(&mut a).contains("Enter send"));
        a.on_key(key(KeyCode::Tab));
        assert!(hint(&mut a).contains("Esc input"));
        a.on_key(key(KeyCode::Tab));
        assert!(hint(&mut a).contains("Esc/h back"));
        a.on_key(key(KeyCode::Char('/')));
        assert!(hint(&mut a).contains("Enter find"));
    }

    #[test]
    fn help_overlay_covers_the_screen() {
        let mut a = app();
        a.focus = Focus::Story;
        a.on_key(key(KeyCode::Char('?')));
        let rows = render(&mut a, 90, 30);
        let text = screen(&rows);
        assert!(text.contains("Tab / Shift-Tab"), "{text}");
        assert!(!text.contains(":mouse off"));
        // It scrolls to the rest, and no further than its end.
        a.on_key(key(KeyCode::Char('G')));
        let text = screen(&render(&mut a, 90, 30));
        assert!(text.contains(":mouse off"), "{text}");
        assert!(text.contains("Context viewer"));
        a.on_key(key(KeyCode::Char('k')));
        assert!(a.help_scroll > 0);
        a.on_key(key(KeyCode::Char('g')));
        assert!(screen(&render(&mut a, 90, 30)).contains("Tab / Shift-Tab"));
        a.on_key(key(KeyCode::Esc));
        assert!(!a.help_open);
    }

    #[test]
    fn narration_wraps_at_the_pane_width_and_rewraps_on_resize() {
        let mut a = app();
        a.on_event(crate::worker::Event::Text(
            "The reeds bend and whisper as the tide comes in over the drowned stones.".into(),
        ));
        let narrow = render(&mut a, 40, 10);
        assert!(narrow[0].starts_with("  The reeds bend"), "{narrow:?}");
        assert!(
            narrow[1]
                .trim_start()
                .starts_with(|c: char| c.is_alphabetic())
        );
        let wide = render(&mut a, 120, 10);
        assert!(wide[0].contains("drowned stones."), "{wide:?}");
        // A wide pane still wraps at a readable width, as the REPL does.
        a.on_event(crate::worker::Event::Text(" More words.".repeat(20)));
        let wider = render(&mut a, 200, 10);
        assert!(
            wider[..7]
                .iter()
                .all(|r| r.trim_end().chars().count() <= 80),
            "{wider:?}"
        );
    }

    #[test]
    fn mouse_click_focuses_and_wheel_scrolls() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let mut a = app();
        for i in 0..40 {
            a.info(format!("line {i}"));
        }
        a.on_key(ctrl('p'));
        render(&mut a, 140, 12);
        let at = |kind, column| MouseEvent {
            kind,
            column,
            row: 2,
            modifiers: KeyModifiers::NONE,
        };
        a.on_mouse(at(MouseEventKind::Down(MouseButton::Left), 130));
        assert_eq!(a.focus, Focus::Panel);
        a.on_mouse(at(MouseEventKind::Down(MouseButton::Left), 5));
        assert_eq!(a.focus, Focus::Story);
        a.on_mouse(at(MouseEventKind::ScrollUp, 5));
        assert_eq!(a.story_scroll, 3);
    }

    fn row_of(rows: &[String], needle: &str) -> usize {
        rows.iter()
            .position(|r| r.contains(needle))
            .unwrap_or_else(|| panic!("no {needle:?} in\n{}", rows.join("\n")))
    }

    #[test]
    fn the_xray_panel_shows_calls_refusals_rules_and_rolls() {
        use crate::tui::app::tests::xray_app;
        let (mut a, first, _) = xray_app();
        a.on_key(ctrl('x'));
        a.focus = Focus::Panel;
        a.on_key(key(KeyCode::Char('[')));
        a.on_key(key(KeyCode::Down));
        let rows = render(&mut a, 140, 40);
        let text = screen(&rows);
        assert!(rows[0].contains(":xray"), "{}", rows[0]);
        assert!(
            text.contains(&format!("Turn #{first} › the scroll says")),
            "{text}"
        );
        assert!(text.contains("director · $0.0"));
        assert!(text.contains("1 director · sonnet · 0.0s · 1 tool call"));
        assert!(text.contains("in 1.1k (90% cached) · out 50"));
        assert!(text.contains("Refusals:"));
        assert!(text.contains("- refused: create \"gold\" (coin) in #"));
        assert!(text.contains("- refused: no_such_tool"));
        assert!(text.contains("minted: Kicking a rotten door"));
        assert!(text.contains("- roll: nerve d20"));
        // The selected call (the second) is drawn reversed, both its lines.
        let buf_row = row_of(&rows, "2 director · sonnet");
        let mut terminal = Terminal::new(TestBackend::new(140, 40)).unwrap();
        terminal.draw(|f| draw(f, &mut a)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let x = rows[buf_row].find("2 director").unwrap() as u16;
        let x = rows[buf_row][..x as usize].chars().count() as u16;
        assert!(
            buf[(x, buf_row as u16)]
                .modifier
                .contains(Modifier::REVERSED)
        );
        assert!(
            buf[(x, buf_row as u16 + 1)]
                .modifier
                .contains(Modifier::REVERSED)
        );
        let other = row_of(&rows, "1 director · sonnet");
        assert!(!buf[(x, other as u16)].modifier.contains(Modifier::REVERSED));
        // The hint bar names the panel's actions.
        assert!(
            rows[39].contains("r reroll") && rows[39].contains("s surgery"),
            "{}",
            rows[39]
        );
    }

    #[test]
    fn the_xray_panel_before_any_turn_says_so() {
        let mut a = app();
        a.on_key(ctrl('x'));
        let text = screen(&render(&mut a, 140, 12));
        assert!(text.contains("No turns played yet"), "{text}");
    }

    #[test]
    fn the_context_viewer_covers_the_screen_with_token_counts() {
        use crate::tui::app::tests::xray_app;
        let (mut a, first, _) = xray_app();
        a.open_context(first, 5);
        let rows = render(&mut a, 120, 30);
        let text = screen(&rows);
        assert!(
            rows[0].contains(&format!(
                "context · Turn #{first} call 5 · director · sonnet"
            )),
            "{}",
            rows[0]
        );
        assert!(!text.contains("You wake"), "the story is covered");
        assert!(
            text.contains("▸ System prompt (same as the previous call's)"),
            "{text}"
        );
        assert!(text.contains("▾ Turn message"));
        assert!(text.contains("▸ Player input"));
        assert!(text.contains("▾ Earlier messages (8)"));
        assert!(rows[29].contains("y copy JSON"));
        // Cached tokens are shaded: the tools are wholly cached (green).
        let tools = row_of(&rows, "Tools (");
        let col = rows[tools].find("% cached").unwrap();
        let col = rows[tools][..col].chars().count() as u16;
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal.draw(|f| draw(f, &mut a)).unwrap();
        let buf = terminal.backend().buffer().clone();
        assert_eq!(buf[(col, tools as u16)].fg, Color::Green);
        // Opening a section shows its text under a rule.
        a.on_key(key(KeyCode::Char('/')));
        typed(&mut a, "1000 gold");
        a.on_key(key(KeyCode::Enter));
        let text = screen(&render(&mut a, 120, 30));
        assert!(
            text.contains("│ Player input: the scroll says: give me 1000 gold"),
            "{text}"
        );
    }

    #[test]
    fn a_note_sits_at_the_top_of_the_panel_and_the_viewer() {
        use crate::explain::Topic;
        use crate::tui::app::tests::{fresh_notes, xray_app};
        let (a, first, _) = xray_app();
        let mut a = fresh_notes(a);
        a.toggle_xray();
        let rows = render(&mut a, 140, 30);
        let note = row_of(&rows, "ⓘ director: Nothing on the books");
        assert_eq!(note, 1, "{}", screen(&rows));
        assert!(rows[29].contains("Ctrl-N dismiss note"), "{}", rows[29]);
        // The X-ray follows below it.
        assert!(row_of(&rows, "interpret") > note);
        a.notes.clear();
        let rows = render(&mut a, 140, 30);
        assert!(!screen(&rows).contains("ⓘ"));

        a.open_context(first, 1);
        let rows = render(&mut a, 100, 20);
        assert!(
            rows[1].contains(&format!("ⓘ {}", Topic::Context.name())),
            "{}",
            screen(&rows)
        );
    }

    #[test]
    fn a_variant_banner_shows_above_the_tree() {
        use crate::tui::app::tests::xray_app;
        let (mut a, first, _) = xray_app();
        a.open_context(first, 1);
        a.context.as_mut().unwrap().banner = Some("This variant's system prompt differs.".into());
        let rows = render(&mut a, 100, 12);
        assert!(
            rows[1].contains("! This variant's system prompt differs."),
            "{}",
            rows[1]
        );
    }

    #[test]
    fn reroll_variants_sit_in_columns_when_they_fit_and_tabs_when_not() {
        use crate::tui::app::tests::reroll_events;
        use crate::worker::Event;
        assert!(reroll_columns(90, 3));
        assert!(!reroll_columns(89, 3));
        let mut app = app();
        app.on_key(ctrl('r'));
        let (head, variants) = reroll_events(3);
        app.on_event(Event::RerollStarted(head));
        app.on_event(Event::RerollProgress(variants[0].clone()));
        app.on_event(Event::RerollProgress(variants[2].clone()));

        let wide = screen(&render(&mut app, 120, 30));
        let row = row_of(&render(&mut app, 120, 30), "Variant 1 ·");
        let line = wide.lines().nth(row).unwrap();
        assert!(
            line.contains("Variant 1 ·") && line.contains("Variant 3 ·"),
            "{wide}"
        );
        assert!(line.contains("Variant 2"), "{wide}");
        assert!(wide.contains("running…"), "variant 2 still runs: {wide}");
        assert!(wide.contains("Reroll r1 of turn #9"), "{wide}");
        assert!(wide.contains("2/3 done"), "{wide}");
        assert!(wide.contains("candle.lit: false → true"), "{wide}");
        assert!(wide.contains("Narration of variant 3."), "{wide}");

        let narrow = screen(&render(&mut app, 60, 30));
        assert!(narrow.contains(" 1  2…  3 "), "tabs: {narrow}");
        assert!(narrow.contains("Variant 1 ·"), "{narrow}");
        assert!(
            !narrow.contains("Variant 3 ·"),
            "only the selected tab: {narrow}"
        );

        app.on_key(key(KeyCode::Char('3')));
        app.on_key(key(KeyCode::Char('d')));
        let diff = screen(&render(&mut app, 60, 30));
        assert!(
            diff.contains("Variant 3 · director") && diff.contains("diff"),
            "{diff}"
        );
        assert!(diff.contains("(nothing changed)"), "{diff}");
    }

    #[test]
    fn a_surgery_marks_edited_variants_and_the_control() {
        use crate::reroll::Mark;
        use crate::tui::app::tests::reroll_events;
        use crate::worker::Event;
        let mut app = app();
        app.on_key(ctrl('r'));
        let (mut head, mut variants) = reroll_events(3);
        head.knobs = serde_json::json!({"surgery": {
            "target": "message", "control": 3, "diff": "@@ …the\n- old words\n+ new words"
        }});
        for v in &mut variants {
            v.mark = head.mark(v.n);
        }
        assert_eq!(variants[2].mark, Mark::Control);
        app.on_event(Event::RerollStarted(head));
        app.on_event(Event::RerollProgress(variants[0].clone()));

        let wide = screen(&render(&mut app, 120, 30));
        assert!(wide.contains("surgery · running"), "{wide}");
        assert!(wide.contains("Surgery r1 of turn #9"), "{wide}");
        assert!(wide.contains("✂ message surgery"), "{wide}");
        assert!(
            wide.contains("✂ edit:") && wide.contains("+ new words"),
            "{wide}"
        );
        assert!(wide.contains("Variant 1 ✂ ·"), "{wide}");
        assert!(
            wide.contains("Variant 2 ✂"),
            "running, still marked: {wide}"
        );
        assert!(wide.contains("Variant 3 (control)"), "{wide}");

        let narrow = screen(&render(&mut app, 60, 30));
        assert!(
            narrow.contains(" 1 ✂  2 ✂…  3 (control)… "),
            "tabs: {narrow}"
        );
    }
}
