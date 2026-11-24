use super::tests::{app, key, typed};
use super::*;

fn command(app: &mut App, line: &str) {
    app.focus = Focus::Input;
    typed(app, line);
    app.on_key(key(KeyCode::Enter));
    app.focus = Focus::Panel;
}

fn selected(app: &App) -> Option<String> {
    let top = app.panel.last()?;
    top.browser
        .as_ref()?
        .links
        .iter()
        .find(|l| Some(l.line) == top.selected)
        .map(|l| l.arg.clone())
}

#[test]
fn categories_entries_sources_search_and_back_use_the_existing_panel() {
    let mut app = app();
    command(&mut app, ":compendium");
    assert_eq!(selected(&app).as_deref(), Some("people"));
    app.on_key(key(KeyCode::Down));
    app.on_key(key(KeyCode::Right));
    assert_eq!(app.panel.last().unwrap().arg, "places");
    let target = selected(&app).unwrap();
    app.on_key(key(KeyCode::Enter));
    let detail = app.panel.last().unwrap();
    assert_eq!(detail.arg, target);
    assert!(detail.text.contains("Brackish water laps at the pews."));
    assert!(selected(&app).unwrap().starts_with("source #"));
    app.on_key(key(KeyCode::Enter));
    assert!(
        app.panel
            .last()
            .unwrap()
            .text
            .contains("You wake in a flooded chapel.")
    );
    assert!(app.panel.last().unwrap().text.starts_with("Narration #"));
    app.on_key(key(KeyCode::Esc));
    assert_eq!(app.panel.last().unwrap().arg, target);
    app.on_key(key(KeyCode::Char('h')));
    assert_eq!(selected(&app).as_deref(), Some(target.as_str()));
    app.on_key(key(KeyCode::Char('/')));
    typed(&mut app, "pews");
    assert!(app.hints().contains("/pews"));
    app.on_key(key(KeyCode::Enter));
    assert!(app.panel.last().unwrap().text.contains("Drowned Chapel"));
    app.on_key(key(KeyCode::Enter));
    assert!(app.panel.last().unwrap().text.contains("entry #"));
    // Selection scrolling and account paging keep using the same panel.
    app.panel_height = 3;
    app.on_key(key(KeyCode::Char(' ')));
    assert!(app.panel.last().unwrap().scroll > 0);
    app.on_key(key(KeyCode::Char('g')));
    assert_eq!(app.panel.last().unwrap().scroll, 0);
    app.on_key(key(KeyCode::Tab));
    assert_eq!(app.focus, Focus::Input);
    command(&mut app, ":brain");
    assert!(app.brain_row().is_some());
    assert!(app.outbox.is_empty());
}

#[test]
fn world_navigation_and_mode_toggle_drop_hidden_targets_and_search() {
    let mut app = app();
    command(&mut app, ":compendium world people");
    assert!(app.panel.last().unwrap().text.contains("Brother Ash"));
    app.on_key(key(KeyCode::Enter));
    assert!(app.panel.last().unwrap().text.contains("bible plan"));
    app.on_key(key(KeyCode::Char('/')));
    typed(&mut app, "ledger");
    app.on_key(key(KeyCode::Enter));
    assert!(app.panel.last().unwrap().text.contains("The bell"));
    app.on_key(key(KeyCode::Enter));
    assert!(
        app.panel
            .last()
            .unwrap()
            .text
            .contains("Hidden bible reference")
    );
    app.on_key(key(KeyCode::Char('m')));
    assert_eq!(app.mode, Mode::Player);
    assert_eq!(app.focus, Focus::Panel);
    assert!(app.search.is_none() && app.last_search.is_none());
    let state = format!("{:?}", app.panel);
    for hidden in [
        "Brother Ash",
        "ledger",
        "The bell",
        "bible.npcs",
        "bible.secrets",
    ] {
        assert!(!state.contains(hidden), "{state}");
    }
    assert_eq!(app.panel.last().unwrap().arg, "");
    assert_eq!(selected(&app).as_deref(), Some("people"));
    assert!(app.outbox.is_empty());
}

#[test]
fn refresh_preserves_targets_and_discards_destroyed_world_selection() {
    let mut app = app();
    command(&mut app, ":compendium world items");
    app.on_key(key(KeyCode::Down)); // the candle, which contains nothing
    let target = selected(&app).unwrap();
    app.refresh_panel();
    assert_eq!(selected(&app).as_deref(), Some(target.as_str()));
    app.on_key(key(KeyCode::Enter));
    let id = target.trim_start_matches('#').parse().unwrap();
    app.view
        .apply_patch(&crate::world::Patch {
            ops: vec![crate::world::Op::Destroy {
                entity: crate::world::EntityRef::Id(id),
            }],
        })
        .unwrap();
    app.refresh_panel();
    assert!(app.panel.last().unwrap().text.contains("No entries found"));
    assert_eq!(selected(&app), None);
    app.on_key(key(KeyCode::Esc));
    assert_ne!(selected(&app).as_deref(), Some(target.as_str()));
    assert!(!app.panel.last().unwrap().text.contains(&target));
}

#[test]
fn shortcuts_open_people_or_mechanics_and_reads_never_queue_work() {
    let mut app = app();
    for (cmd, heading) in [
        (":npcs", "People"),
        (":rules", "Mechanics"),
        (":glossary", "Mechanics"),
    ] {
        command(&mut app, cmd);
        assert!(app.panel.last().unwrap().text.starts_with(heading));
    }
    assert!(app.outbox.is_empty());
}

#[test]
fn browser_renders_in_wide_panel_and_narrow_overlay() {
    use ratatui::{Terminal, backend::TestBackend};
    for (width, height) in [(140, 40), (80, 24)] {
        let mut app = app();
        command(&mut app, ":compendium");
        let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
        term.draw(|f| crate::tui::view::draw(f, &mut app)).unwrap();
        assert_eq!(app.areas.story.is_none(), width == 80);
        let text = format!("{}", term.backend());
        assert!(text.contains("Player"));
        assert!(text.contains("Places (1)"));
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Enter));
        app.on_key(key(KeyCode::Enter));
        term.draw(|f| crate::tui::view::draw(f, &mut app)).unwrap();
        assert!(format!("{}", term.backend()).contains("Brackish water"));
        app.on_key(key(KeyCode::Esc));
        assert_eq!(app.panel.last().unwrap().arg, "places");
    }
}

#[test]
fn switching_saves_discards_player_history_and_in_progress_search_too() {
    use crate::branches::tests::{saved_game, temp_dir};
    let dir = temp_dir("browser-player-switch");
    let main = saved_game(&dir, "mill.db");
    let world = World::open(&main).unwrap();
    let other = dir.join("other.db");
    drop(world.fork(world.last_seq().unwrap(), &other).unwrap());
    drop(world);
    let mut app = App::new(World::open_read_only(&main).unwrap(), true);
    command(&mut app, ":compendium drowned");
    command(&mut app, ":quests world history");
    app.on_key(key(KeyCode::Char('/')));
    typed(&mut app, "Drowned Chapel");
    app.switch_to(other);
    assert_eq!(app.mode, Mode::Player);
    assert!(app.search.is_none() && app.last_search.is_none());
    assert!(app.panel.iter().all(|v| v.browser.is_none()));
    assert!(!app.panel_open);
    assert_eq!(app.focus, Focus::Input);
    drop(app);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn mode_switch_then_back_cannot_restore_rows_from_the_other_projection() {
    let mut app = app();
    command(&mut app, ":time");
    command(&mut app, ":compendium places");
    app.on_key(key(KeyCode::Char('m')));
    assert_eq!(app.mode, Mode::World);
    assert!(
        app.panel
            .iter()
            .all(|v| v.browser.is_none() || v.mode == Mode::World)
    );
    app.on_key(key(KeyCode::Esc));
    assert_eq!(app.panel.last().unwrap().cmd, ":time");
    command(&mut app, ":compendium");
    app.on_key(key(KeyCode::Down));
    app.on_key(key(KeyCode::Enter));
    let target = selected(&app).unwrap();
    app.on_key(key(KeyCode::Enter));
    assert_eq!(app.panel.last().unwrap().arg, target);
    assert!(app.panel.last().unwrap().text.contains("World mode"));
    app.on_key(key(KeyCode::Char('m')));
    app.on_key(key(KeyCode::Esc));
    assert_eq!(app.panel.last().unwrap().cmd, ":time");
    assert!(app.panel.iter().all(|v| v.browser.is_none()));
}

#[test]
fn refresh_keeps_exact_entry_after_rename_ambiguity_and_removal() {
    use crate::world::{Op, Patch};
    let mut app = app();
    command(&mut app, ":compendium world Drowned Chapel");
    let target = app.panel.last().unwrap().arg.clone();
    let id: i64 = target.trim_start_matches('#').parse().unwrap();
    app.view
        .apply_engine_patch(&Patch::new(vec![
            Op::Rename {
                entity: id.into(),
                name: "Restored Chapel".into(),
            },
            Op::Create {
                handle: None,
                name: "Drowned Chapel".into(),
                kind: "room".into(),
                location: None,
                props: Default::default(),
            },
            Op::Create {
                handle: None,
                name: "Restored Chapel".into(),
                kind: "room".into(),
                location: None,
                props: Default::default(),
            },
        ]))
        .unwrap();
    app.on_event(Event::TickDone {
        forced: false,
        outcome: Ok(crate::game::TickOutcome {
            summary: String::new(),
            changes: vec![],
            calls: 0,
        }),
    });
    assert_eq!(app.panel.last().unwrap().arg, target);
    assert!(
        app.panel
            .last()
            .unwrap()
            .text
            .starts_with("Restored Chapel")
    );
    assert!(!app.panel.last().unwrap().text.contains("No entries found"));
    // Removing a learned subject preserves its known account; removing a
    // World entity drops its detail without resolving an old name again.
    command(&mut app, ":compendium world tallow candle");
    let target = app.panel.last().unwrap().arg.clone();
    let id: i64 = target.trim_start_matches('#').parse().unwrap();
    app.view
        .apply_engine_patch(&Patch::new(vec![Op::Destroy { entity: id.into() }]))
        .unwrap();
    app.view
        .apply_engine_patch(&Patch::new(vec![Op::Create {
            handle: None,
            name: "tallow candle".into(),
            kind: "item".into(),
            location: None,
            props: Default::default(),
        }]))
        .unwrap();
    for _ in 0..2 {
        app.refresh_panel();
    }
    assert_eq!(app.panel.last().unwrap().arg, target);
    assert!(app.panel.last().unwrap().text.contains("No entries found"));
}

#[test]
fn switching_saves_clears_world_search_without_any_world_view_to_drop() {
    use crate::branches::tests::{saved_game, temp_dir};
    let dir = temp_dir("browser-world-search-switch");
    let main = saved_game(&dir, "mill.db");
    let world = World::open(&main).unwrap();
    let other = dir.join("other.db");
    drop(world.fork(world.last_seq().unwrap(), &other).unwrap());
    drop(world);
    let mut app = App::new(World::open_read_only(&main).unwrap(), true);
    command(&mut app, ":compendium world people");
    app.on_key(key(KeyCode::Esc));
    command(&mut app, ":time");
    assert_eq!(app.mode, Mode::World);
    assert!(app.panel.iter().all(|v| !commands::is_moded(&v.cmd)));

    // Search a mode-independent panel while the session remains in World.
    app.on_key(key(KeyCode::Char('/')));
    typed(&mut app, "Brother Ash");
    app.on_key(key(KeyCode::Enter));
    assert_eq!(app.last_search, Some((Focus::Panel, "Brother Ash".into())));
    app.on_key(key(KeyCode::Char('/')));
    typed(&mut app, "The hidden bell");
    assert_eq!(app.search.as_ref().unwrap().query, "The hidden bell");

    app.switch_to(other.clone());
    assert_eq!(app.mode, Mode::Player);
    assert!(app.search.is_none() && app.last_search.is_none());
    assert_eq!(app.panel.last().unwrap().cmd, ":time");
    for text in [app.hints(), format!("{:?}", app.panel)] {
        assert!(!text.contains("Brother Ash") && !text.contains("The hidden bell"));
    }
    assert_eq!(app.outbox, [Command::Switch(other)]);
    drop(app);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn quest_lists_details_links_search_and_toggle_use_the_browser() {
    let mut app = app();
    let ids = crate::knowledge::tests::quests::populate(&mut app.view);
    command(&mut app, ":quests");
    assert_eq!(selected(&app), Some(format!("#{}", ids[0])));
    app.on_key(key(KeyCode::Down));
    app.on_key(key(KeyCode::Enter));
    assert_eq!(app.panel.last().unwrap().cmd, ":quests");
    assert!(
        app.panel
            .last()
            .unwrap()
            .text
            .contains("Goal: Bring the bell home.")
    );
    app.on_key(key(KeyCode::Esc));
    assert_eq!(selected(&app), Some(format!("#{}", ids[1])));
    app.on_key(key(KeyCode::Char('/')));
    typed(&mut app, "nightfall");
    app.on_key(key(KeyCode::Enter));
    assert_eq!(app.panel.last().unwrap().cmd, ":quests");
    assert!(
        app.panel
            .last()
            .unwrap()
            .text
            .contains("Bring the bell home 3")
    );
    command(&mut app, &format!(":quests world #{}", ids[1]));
    assert!(
        app.panel
            .last()
            .unwrap()
            .text
            .contains("Hidden village destruction")
    );
    app.on_key(key(KeyCode::Char('m')));
    assert_eq!(app.mode, Mode::Player);
    assert_eq!(app.panel.last().unwrap().cmd, ":quests");
    assert!(!format!("{:?}", app.panel).contains("Hidden"));
    assert!(app.search.is_none() && app.last_search.is_none());
    app.on_key(key(KeyCode::Esc));
    assert!(!app.panel_open, "hidden history cannot be restored");
    assert!(app.outbox.is_empty());
}

#[test]
fn quests_refresh_after_ticks_and_render_in_panel_and_overlay() {
    use ratatui::{Terminal, backend::TestBackend};
    for (width, height) in [(140, 40), (80, 24)] {
        let mut app = app();
        let ids = crate::knowledge::tests::quests::populate(&mut app.view);
        for arg in [
            "ongoing".to_string(),
            "history".to_string(),
            format!("#{}", ids[1]),
        ] {
            command(&mut app, &format!(":quests {arg}"));
            let mut term = Terminal::new(TestBackend::new(width, height)).unwrap();
            term.draw(|f| crate::tui::view::draw(f, &mut app)).unwrap();
            let text = format!("{}", term.backend());
            assert_eq!(app.areas.story.is_none(), width == 80);
            assert!(text.contains("Player"));
            assert!(text.contains("Bring the bell home"));
            assert!(!text.contains("Hidden") && !text.contains("Private"));
            if arg.starts_with('#') {
                assert!(text.contains("before nightfall"));
            }
        }
        command(&mut app, ":quests");
        let id = ids[0];
        let mut state = app.view.entity(id).unwrap().unwrap().props["_quest"].clone();
        state["status"] = serde_json::json!("complete");
        app.view
            .apply_engine_patch(&crate::world::Patch::new(vec![crate::world::Op::SetProp {
                entity: id.into(),
                key: "_quest".into(),
                value: state,
            }]))
            .unwrap();
        app.on_event(Event::TickDone {
            forced: false,
            outcome: Ok(crate::game::TickOutcome {
                summary: String::new(),
                changes: vec![],
                calls: 0,
            }),
        });
        assert!(
            !app.panel
                .last()
                .unwrap()
                .text
                .contains("Bring the bell home 0")
        );
        assert_eq!(selected(&app), Some(format!("#{}", ids[1])));
        command(&mut app, ":quests history");
        assert!(
            app.panel
                .last()
                .unwrap()
                .text
                .contains("Bring the bell home 0")
        );
    }
}

#[test]
fn a_quest_related_entry_opens_the_compendium_and_back_returns_to_the_quest() {
    use crate::knowledge::{Discovery, Found};
    use serde_json::json;
    let mut app = app();
    let ids = crate::knowledge::tests::quests::populate(&mut app.view);
    let Found::One(chapel) = crate::knowledge::find(&app.view, "Drowned Chapel").unwrap() else {
        panic!()
    };
    let Found::One(account) = crate::knowledge::find(&app.view, "Bring the bell home 1").unwrap()
    else {
        panic!()
    };
    let source = app
        .view
        .log_event(
            crate::world::EventKind::Narration,
            &json!({"text": "The chapel shelters the bell."}),
        )
        .unwrap();
    let d: Discovery = serde_json::from_value(json!({"entry": account.id, "facts": [{"text": "The chapel shelters the bell.", "excerpt": "The chapel shelters the bell", "source": source, "related": [chapel.id]}]})).unwrap();
    crate::knowledge::learn(&mut app.view, &d, None)
        .unwrap()
        .unwrap();
    command(&mut app, &format!(":quests #{}", ids[1]));
    app.on_key(key(KeyCode::Enter));
    assert_eq!(app.panel.last().unwrap().cmd, ":compendium");
    assert_eq!(app.panel.last().unwrap().arg, format!("#{}", chapel.id));
    app.on_key(key(KeyCode::Esc));
    assert_eq!(app.panel.last().unwrap().cmd, ":quests");
    assert_eq!(app.panel.last().unwrap().arg, format!("#{}", ids[1]));
    assert!(app.outbox.is_empty());
}
