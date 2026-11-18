//! Drawing the TUI: story, side panel, status bar, input line, hint bar,
//! and the help overlay.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use super::app::{App, Areas, Focus, KEYS, Kind, PROMPT};

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
    if app.help_open {
        draw_help(frame, frame.area());
    }
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
    app.panel_width = inner.width as usize;
    app.panel_height = inner.height as usize;
    let lines = app.panel_lines(inner.width as usize);
    let scroll = app
        .panel
        .last()
        .map_or(0, |v| v.scroll)
        .min(lines.len().saturating_sub(1));
    let query = search_query(app, Focus::Panel);
    let selected = app.panel.last().and_then(|v| v.selected);
    let shown: Vec<Line> = lines
        .into_iter()
        .enumerate()
        .skip(scroll)
        .take(inner.height as usize)
        .map(|(i, text)| {
            let hit = query
                .as_ref()
                .is_some_and(|q| text.to_lowercase().contains(q));
            if selected == Some(i) {
                Line::styled(text, Style::new().add_modifier(Modifier::REVERSED))
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

fn draw_help(frame: &mut Frame, area: Rect) {
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .title(" keys — Esc closes ")
        .border_style(Style::new().fg(Color::Cyan));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(KEYS), inner);
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
        let rows = render(&mut a, 90, 40);
        let text = screen(&rows);
        assert!(text.contains("Tab / Shift-Tab"), "{text}");
        assert!(text.contains(":mouse off"));
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
}
