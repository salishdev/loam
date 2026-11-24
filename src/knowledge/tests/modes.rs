//! Player and World mode: two projections of one save, neither writing.

use super::*;
use crate::commands::{mode_switch, report_in};

const HIDDEN_PROP: &str = "drowned the bell himself";

/// `:compendium <arg>` typed in session mode `mode`.
fn read(world: &World, mode: Mode, arg: &str) -> String {
    let (mode, arg) = commands::resolve_mode(world, mode, ":compendium", arg).unwrap();
    report_in(world, mode, ":compendium", &arg)
        .expect(":compendium is a report command")
        .expect("the report reads")
}

/// The tower game, plus things the player never met: Brother Ash (the
/// bible's NPC) made real in a room the player never saw, carrying a
/// hidden prop and a lantern.
fn with_hidden_world() -> (Game, EntityId, EntityId) {
    let (mut game, _) = in_tower();
    let made = game
        .world_mut()
        .apply_patch(
            &serde_json::from_value(json!({"ops": [
                {"op": "create", "handle": "ash", "name": "Brother Ash", "kind": "monk",
                 "props": {"acts": true, "guilt": HIDDEN_PROP}},
                {"op": "create", "handle": "lamp", "name": "storm lantern", "kind": "item",
                 "location": "ash", "props": {"lit": true}}
            ]}))
            .unwrap(),
        )
        .unwrap()
        .created;
    (game, made[0], made[1])
}

/// Every Player-mode report a player could ask for about `names`.
fn player_reports(world: &World, names: &[&str]) -> Vec<String> {
    let mut out = vec![read(world, Mode::Player, "")];
    for c in Category::ALL {
        out.push(read(world, Mode::Player, c.heading()));
    }
    for e in entries(world).unwrap() {
        out.push(read(world, Mode::Player, &format!("#{}", e.id)));
    }
    for n in names {
        out.push(read(world, Mode::Player, n));
        out.push(read(world, Mode::Player, &format!("search {n}")));
    }
    out
}

#[test]
fn player_shows_what_was_learned_and_world_shows_the_world_and_the_bible() {
    let (game, ash, lamp) = with_hidden_world();
    let world = game.world();

    let player = read(world, Mode::Player, "");
    assert!(player.starts_with("Compendium · Player mode"), "{player}");
    assert!(player.contains("Drowned Chapel (place)"), "{player}");
    assert!(player.contains("Bell Tower (place)"), "{player}");
    for hidden in [
        "Brother Ash",
        "storm lantern",
        "altar",
        "tallow candle",
        "crow",
        "Eel-wives",
        "Saltmarsh",
        "The bell",
        "People",
        "Mechanics",
    ] {
        assert!(!player.contains(hidden), "{hidden} leaked:\n{player}");
    }

    let overview = read(world, Mode::World, "");
    assert!(
        overview.starts_with("Compendium · World mode"),
        "{overview}"
    );
    let crow = world
        .entities_marked(crate::world::ACTS)
        .unwrap()
        .into_iter()
        .find(|&id| id != ash)
        .unwrap();
    for line in [
        format!("#{ash} Brother Ash (person · world, in bible)"),
        format!("#{crow} crow (person · world)"),
        "bible.factions.1 Eel-wives (faction · bible plan)".into(),
        "bible.regions.1 Saltmarsh (place · bible plan)".into(),
        "(place · world)".into(),
        format!("#{lamp} storm lantern (item · world)"),
        "tallow candle (item · world)".into(),
        "bible.secrets.1 The bell (lore · bible reference)".into(),
        "bible.tone Tone (lore · bible reference)".into(),
        // The tower's unexplored way up: a stored stub, not yet a place.
        "(place · world, unexplored)".into(),
        "People (2 in the world, 0 in the bible only)".into(),
    ] {
        assert!(overview.contains(&line), "{line} missing:\n{overview}");
    }
    // Every stored rule and glossary term, as the engine holds it.
    let rules = crate::rules::all(world).unwrap();
    assert!(!rules.is_empty());
    for r in &rules {
        let line = format!("#{} {} (mechanic · world)", r.id, r.def.name);
        assert!(overview.contains(&line), "{line} missing:\n{overview}");
        let detail = read(world, Mode::World, &format!("#{}", r.id));
        assert!(
            detail.contains(&format!("when: {}", r.def.trigger)),
            "{detail}"
        );
    }
    for term in crate::glossary::terms(world).unwrap().keys() {
        let line = format!("glossary.{term} {term} (mechanic · world)");
        assert!(overview.contains(&line), "{line} missing:\n{overview}");
    }

    // Details read stored state, hidden props and bible notes included.
    let ash_detail = read(world, Mode::World, "brother ash");
    assert!(ash_detail.contains(HIDDEN_PROP), "{ash_detail}");
    assert!(
        ash_detail.contains("Last monk, lies kindly."),
        "{ash_detail}"
    );
    assert!(
        ash_detail.contains(&format!("holds: #{lamp} storm lantern")),
        "{ash_detail}"
    );
    let lamp_detail = read(world, Mode::World, &format!("#{lamp}"));
    assert!(
        lamp_detail.contains(&format!("in #{ash} Brother Ash")),
        "{lamp_detail}"
    );
    let secret = read(world, Mode::World, "bible.secrets.1");
    assert!(
        secret.contains("The abbot drowned the bell to hide the ledger"),
        "{secret}"
    );
    assert!(secret.contains("Hidden bible reference"), "{secret}");
    assert!(
        read(world, Mode::World, "search ledger").contains("bible.secrets.1 The bell"),
        "World search reaches the bible"
    );
}

#[test]
fn a_bible_npc_without_an_entity_is_a_plan_not_world_state() {
    let (game, _) = in_tower();
    let world = game.world();
    let people = read(world, Mode::World, "people");
    assert!(people.starts_with("People · World mode"), "{people}");
    assert!(
        people.contains("bible.npcs.1 Brother Ash (person · bible plan)"),
        "{people}"
    );
    let ash = read(world, Mode::World, "brother ash");
    assert!(ash.contains("Last monk, lies kindly."), "{ash}");
    assert!(ash.contains("not instantiated"), "{ash}");
}

#[test]
fn player_mode_filters_before_search_names_and_counts() {
    let (plain, _) = in_tower();
    let (hidden, ash, lamp) = with_hidden_world();
    let probes = [
        "Brother Ash",
        "ash",
        "storm lantern",
        "lantern",
        HIDDEN_PROP,
        "guilt",
        "crow",
        "wary",
        "altar",
        "ledger",
        "Eel-wives",
    ];
    // Hidden world state changes nothing a Player report says: not a
    // label, a snippet, a count or a lookup.
    let before = player_reports(plain.world(), &probes);
    let after = player_reports(hidden.world(), &probes);
    assert_eq!(before, after);
    for text in &after {
        for word in ["Brother", "lantern", HIDDEN_PROP, "Saltmarsh"] {
            // The probes echo the player's own words; nothing else names
            // them.
            let echoed = probes.iter().any(|p| text.contains(&format!("{p:?}")));
            assert!(echoed || !text.contains(word), "{word} leaked:\n{text}");
        }
    }
    for id in [ash, lamp] {
        let text = read(hidden.world(), Mode::Player, &format!("#{id}"));
        assert!(text.starts_with("You know of nothing called"), "{text}");
    }
    assert!(find(hidden.world(), "Brother Ash").unwrap() == Found::None);
    assert!(search(hidden.world(), "lantern").unwrap().is_empty());
}

#[test]
fn an_explicit_mode_argument_overrides_the_session_mode() {
    let (game, _, _) = with_hidden_world();
    let world = game.world();
    assert_eq!(
        read(world, Mode::Player, "world"),
        read(world, Mode::World, "")
    );
    assert_eq!(
        read(world, Mode::World, "player"),
        read(world, Mode::Player, "")
    );
    assert_eq!(
        read(world, Mode::Player, "WORLD people"),
        read(world, Mode::World, "people")
    );
    assert_eq!(
        read(world, Mode::World, "player search tower"),
        read(world, Mode::Player, "search tower")
    );
    // Plain `:compendium` (no session mode) is Player.
    assert_eq!(
        commands::report(world, ":compendium", "").unwrap().unwrap(),
        read(world, Mode::Player, "")
    );
    assert_eq!(mode_switch("", Mode::Player), Ok(Mode::World));
    assert_eq!(mode_switch("", Mode::World), Ok(Mode::Player));
    assert_eq!(mode_switch("player", Mode::World), Ok(Mode::Player));
    assert!(mode_switch("god", Mode::Player).is_err());
}

#[test]
fn reading_either_mode_calls_no_model_and_writes_nothing() {
    let (game, _, _) = with_hidden_world();
    let world = game.world();
    let state = world.state().unwrap();
    let events = world.events().unwrap();
    let calls = world.last_call_id().unwrap();
    let known = entries(world).unwrap();
    for mode in [Mode::Player, Mode::World, Mode::Player] {
        for arg in [
            "",
            "people",
            "mechanics",
            "lore",
            "brother ash",
            "#1",
            "search bell",
        ] {
            read(world, mode, arg);
        }
    }
    world_entries(world).unwrap();
    assert_eq!(world.state().unwrap(), state);
    assert_eq!(
        world.events().unwrap(),
        events,
        "no discovery or other event"
    );
    assert_eq!(world.last_call_id().unwrap(), calls, "no model call");
    assert_eq!(entries(world).unwrap(), known, "knowledge unchanged");
}

#[test]
fn a_mode_word_before_a_name_selects_a_mode_unless_the_name_is_known_in_the_current_one() {
    let (mut game, _, _) = with_hidden_world();
    game.world_mut()
        .apply_patch(
            &serde_json::from_value(json!({"ops": [
                {"op": "create", "name": "World Tree", "kind": "tree"}
            ]}))
            .unwrap(),
        )
        .unwrap();
    let world = game.world();
    // Nothing learned is called "world brother ash": World mode, once.
    assert_eq!(
        read(world, Mode::Player, "world brother ash"),
        read(world, Mode::World, "brother ash")
    );
    let resolved = commands::resolve_mode(world, Mode::Player, ":compendium", "world tower");
    assert_eq!(resolved, Ok((Mode::World, "tower".to_owned())));
    // A name that begins with a mode word stays a name where it is known.
    let resolved = commands::resolve_mode(world, Mode::World, ":compendium", "world tree");
    assert_eq!(resolved, Ok((Mode::World, "world tree".to_owned())));
    assert!(read(world, Mode::World, "world tree").starts_with("World Tree · item"));
    let other = commands::resolve_mode(world, Mode::Player, ":time", "world");
    assert_eq!(other, Ok((Mode::Player, "world".to_owned())));
}

#[test]
fn a_mode_word_selects_a_mode_only_before_a_section_search_or_key() {
    for (arg, mode, rest) in [
        ("world", Some(Mode::World), ""),
        ("World people", Some(Mode::World), "people"),
        (
            "player search the bell",
            Some(Mode::Player),
            "search the bell",
        ),
        ("world #12", Some(Mode::World), "#12"),
        ("world bible.npcs.1", Some(Mode::World), "bible.npcs.1"),
        ("world glossary.lit", Some(Mode::World), "glossary.lit"),
        // The start of a name, not a mode.
        ("world tree", None, "world tree"),
        ("player world", None, "player world"),
        ("world people of the fen", None, "world people of the fen"),
    ] {
        assert_eq!(split_mode(arg), (mode, rest), "{arg}");
        // What is left never selects a mode again.
        assert_eq!(split_mode(rest).0, None, "{arg}");
    }
}

#[test]
fn the_npcs_shortcut_opens_people_in_the_selected_mode() {
    let (game, _, _) = with_hidden_world();
    let world = game.world();
    for mode in [Mode::Player, Mode::World] {
        assert_eq!(
            report_in(world, mode, ":npcs", "").unwrap().unwrap(),
            read(world, mode, "people")
        );
    }
    assert!(
        !commands::report(world, ":npcs", "")
            .unwrap()
            .unwrap()
            .contains("Brother Ash")
    );
}

#[test]
fn the_mechanics_shortcuts_read_learned_or_engine_mechanics_by_mode() {
    let (game, _, _) = with_hidden_world();
    let world = game.world();
    let engine = read(world, Mode::World, "mechanics");
    let rule = &crate::rules::all(world).unwrap()[0];
    assert!(engine.contains(&rule.def.name), "{engine}");
    for cmd in [":rules", ":glossary"] {
        assert_eq!(
            report_in(world, Mode::World, cmd, "").unwrap().unwrap(),
            engine
        );
        let learned = report_in(world, Mode::Player, cmd, "").unwrap().unwrap();
        assert_eq!(learned, commands::report(world, cmd, "").unwrap().unwrap());
        assert!(
            !learned.contains(&rule.def.name),
            "{cmd} leaked:\n{learned}"
        );
    }
}
