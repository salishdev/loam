use serde_json::json;

use super::*;
use crate::commands;
use crate::game::Game;
use crate::game::tests::begun;
use crate::llm::{Response, ScriptedClient};
use crate::world::{EntityRef, Event};

fn compendium(world: &World, arg: &str) -> String {
    commands::report(world, ":compendium", arg)
        .expect(":compendium is a report command")
        .expect("the report reads")
}

fn names(world: &World) -> Vec<String> {
    entries(world)
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect()
}

fn player_room(game: &Game) -> EntityId {
    let world = game.world();
    let player = world.entities_of_kind(crate::world::PLAYER_KIND).unwrap()[0];
    world.entity(player).unwrap().unwrap().location.unwrap()
}

fn tower_room() -> Response {
    Response::canned_tool_use(
        "r2",
        "create_room",
        json!({"via_exit": "north", "back_exit": "south", "name": "Bell Tower", "region": "Saltmarsh",
               "description": "A leaning tower.", "exits": {"up": "rotten stairs"},
               "contents": [{"name": "crow", "kind": "crow", "props": {"mood": "wary", "acts": true}}]}),
    )
}

fn move_player(exit: &str) -> Response {
    Response::canned_tool_use("m", "move_player", json!({ "exit": exit }))
}

/// A begun game where the player went north into the Bell Tower.
fn in_tower() -> (Game, ScriptedClient) {
    let (mut game, client) = begun();
    client.push(tower_room());
    client.push(Response::canned_text("You climb into the bell tower."));
    game.turn(&client, "go north", &mut |_| {}).unwrap();
    (game, client)
}

fn source(world: &World, seq: i64) -> Event {
    world.event(seq).unwrap().expect("source event exists")
}

fn place(world: &World, name: &str) -> Entry {
    match find(world, name).unwrap() {
        Found::One(e) => e,
        other => panic!("expected one entry for {name:?}, got {other:?}"),
    }
}

mod browser;
mod encounters;
mod learned;
mod mechanics;
mod modes;

#[test]
fn the_opening_and_a_room_entry_make_readable_place_entries_with_narration_sources() {
    let (game, _) = in_tower();
    let world = game.world();
    assert_eq!(names(world), ["Drowned Chapel", "Bell Tower"]);

    let chapel = place(world, "Drowned Chapel");
    assert_eq!(chapel.category, Category::Place);
    assert_eq!(chapel.summary, "Brackish water laps at the pews.");
    let opening = source(world, chapel.first_seen);
    assert_eq!(opening.kind, EventKind::Narration);
    assert_eq!(opening.payload["text"], "You wake in a flooded chapel.");

    let tower = place(world, "bell tower");
    assert_eq!(tower.summary, "A leaning tower.");
    assert_eq!(
        source(world, tower.last_seen).payload["text"],
        "You climb into the bell tower."
    );
    assert_eq!(
        tower.facts,
        [Fact {
            text: "Reached from Drowned Chapel.".into(),
            kind: FactKind::Observed,
            claimed_by: None,
            claimant: None,
            sources: vec![Source::event(tower.first_seen)],
            supersedes: Vec::new(),
            rule_version: None,
        }]
    );
    // Every cited source is narration the player saw.
    for e in entries(world).unwrap() {
        let cited = [e.first_seen, e.last_seen, e.summary_source]
            .into_iter()
            .chain(
                e.facts
                    .iter()
                    .flat_map(|f| f.sources.iter().map(|s| s.event)),
            );
        for seq in cited {
            assert_eq!(source(world, seq).kind, EventKind::Narration);
        }
    }

    // The shared report command: what plain mode prints and the TUI panel
    // shows.
    let overview = compendium(world, "");
    assert!(overview.contains("Places (2)"), "{overview}");
    assert!(overview.contains(&format!("#{} Drowned Chapel (place)", chapel.id)));
    assert!(overview.contains(&format!("#{} Bell Tower (place)", tower.id)));
    assert_eq!(compendium(world, "places"), compendium(world, "Places"));
    let detail = compendium(world, "bell");
    assert!(detail.starts_with(&format!("Bell Tower · place · entry #{}", tower.id)));
    assert!(detail.contains(&format!("A leaning tower. [#{}]", tower.last_seen)));
    assert!(detail.contains(&format!(
        "1. Reached from Drowned Chapel. [#{}]",
        tower.first_seen
    )));
    assert_eq!(compendium(world, &format!("#{}", tower.id)), detail);
    assert!(compendium(world, "search pews").contains("Drowned Chapel"));
}

#[test]
fn player_queries_reveal_only_places_the_player_entered() {
    let (mut game, client) = in_tower();
    // An unvisited room behind the tower's stairs, with an exit and a
    // hidden prop: made by the engine, never entered.
    let tower = player_room(&game);
    let crypt = game
        .world_mut()
        .apply_engine_patch(&Patch::new(vec![
            Op::Create {
                handle: Some("c".into()),
                name: "Hidden Crypt".into(),
                kind: ROOM_KIND.into(),
                location: None,
                props: [("description".into(), json!("Bones under the bell."))]
                    .into_iter()
                    .collect(),
            },
            Op::Relate {
                subject: tower.into(),
                rel: crate::world::EXIT_REL.into(),
                object: EntityRef::Handle("c".into()),
                props: [("dir".into(), json!("down"))].into_iter().collect(),
            },
            Op::SetProp {
                entity: tower.into(),
                key: "haunted_by".into(),
                value: json!("the drowned abbot"),
            },
        ]))
        .unwrap()
        .created[0];
    // An off-screen tick, and a director patch on the crypt during a turn
    // that stays put, discover nothing.
    client.push(Response::canned_text("Somewhere, a bell tolls underwater."));
    game.tick(&client).unwrap();
    client.push(Response::canned_tool_use(
        "p",
        "apply_patch",
        json!({"ops": [{"op": "set_prop", "entity": crypt, "key": "flooded", "value": true}]}),
    ));
    client.push(Response::canned_text("You listen to the wind."));
    game.turn(&client, "listen", &mut |_| {}).unwrap();

    let world = game.world();
    assert_eq!(names(world), ["Drowned Chapel", "Bell Tower"]);
    for hidden in [
        "Hidden Crypt",
        "crypt",
        "Bones",
        "abbot",
        "haunted",
        "rotten stairs",
        "Brother Ash",
        "Eel-wives",
        "Saltmarsh",
        "ledger",
        "crow",
        "wary",
        "slate",
        "candle",
    ] {
        assert_eq!(find(world, hidden).unwrap(), Found::None, "{hidden}");
        assert!(search(world, hidden).unwrap().is_empty(), "{hidden}");
        let shown = [
            compendium(world, ""),
            compendium(world, "places"),
            compendium(world, hidden),
            compendium(world, &format!("search {hidden}")),
            compendium(world, "Bell Tower"),
            compendium(world, "Drowned Chapel"),
        ]
        .join("\n")
        // A miss repeats the query back; nothing else may mention it.
        .replace(&format!("{hidden:?}"), "");
        assert!(
            !shown.to_lowercase().contains(&hidden.to_lowercase()),
            "{hidden} leaked:\n{shown}"
        );
    }
    // A world entity's id is not an entry id.
    assert_eq!(find(world, &format!("#{crypt}")).unwrap(), Found::None);
    assert_eq!(find(world, &format!("#{tower}")).unwrap(), Found::None);
}

#[test]
fn a_remembered_place_outlives_changes_to_the_room_and_reentry_keeps_one_identity() {
    let (mut game, client) = in_tower();
    let tower = player_room(&game);
    let before = place(game.world(), "Bell Tower");
    // Back to the chapel: re-entering it updates the chapel's entry.
    client.push(move_player("south"));
    client.push(Response::canned_text("Back among the pews."));
    game.turn(&client, "go south", &mut |_| {}).unwrap();
    let chapel = place(game.world(), "Drowned Chapel");
    assert_eq!(chapel.encounters, 2);
    assert_eq!(chapel.facts[0].text, "Reached from Bell Tower.");
    assert_eq!(entries(game.world()).unwrap().len(), 2);

    // Off-screen, the tower is renamed, redescribed, then destroyed.
    let crow = game
        .world()
        .contents(Some(tower))
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    game.world_mut()
        .apply_engine_patch(&Patch::new(vec![
            Op::Rename {
                entity: tower.into(),
                name: "Fallen Tower".into(),
            },
            Op::SetProp {
                entity: tower.into(),
                key: "description".into(),
                value: json!("Rubble."),
            },
        ]))
        .unwrap();
    assert_eq!(place(game.world(), "Bell Tower"), before);
    game.world_mut()
        .apply_engine_patch(&Patch::new(vec![
            Op::Destroy {
                entity: crow.into(),
            },
            Op::Destroy {
                entity: tower.into(),
            },
        ]))
        .unwrap();
    assert!(game.world().entity(tower).unwrap().is_none());
    assert_eq!(place(game.world(), "Bell Tower"), before);
    assert!(compendium(game.world(), "Bell Tower").contains("A leaning tower."));
    assert_eq!(find(game.world(), "Fallen").unwrap(), Found::None);
}

#[test]
fn going_back_and_forth_keeps_one_entry_per_place() {
    let (mut game, client) = in_tower();
    for (exit, text) in [("south", "Pews."), ("north", "Tower."), ("south", "Pews.")] {
        client.push(move_player(exit));
        client.push(Response::canned_text(text));
        game.turn(&client, &format!("go {exit}"), &mut |_| {})
            .unwrap();
    }
    let world = game.world();
    assert_eq!(names(world), ["Drowned Chapel", "Bell Tower"]);
    let chapel = place(world, "Drowned Chapel");
    assert_eq!(chapel.encounters, 3);
    assert_eq!(chapel.facts.len(), 1, "one fact cited twice");
    assert_eq!(chapel.facts[0].sources.len(), 2);
    assert_eq!(place(world, "Bell Tower").encounters, 2);
}

#[test]
fn same_named_rooms_stay_separate_entries() {
    let (mut game, client) = begun();
    client.push(Response::canned_tool_use(
        "r2",
        "create_room",
        json!({"via_exit": "north", "back_exit": "south", "name": "Drowned Chapel", "region": "Saltmarsh",
               "description": "A second, smaller chapel.", "exits": {}, "contents": []}),
    ));
    client.push(Response::canned_text("Another chapel."));
    game.turn(&client, "go north", &mut |_| {}).unwrap();
    let world = game.world();
    assert_eq!(names(world), ["Drowned Chapel", "Drowned Chapel"]);
    assert!(matches!(find(world, "drowned chapel").unwrap(), Found::Many(m) if m.len() == 2));
    assert!(compendium(world, "Drowned Chapel").contains("could be several entries"));
}

#[test]
fn director_patches_cannot_touch_knowledge() {
    let (mut game, _) = in_tower();
    let entry = place(game.world(), "Bell Tower").id;
    let room = player_room(&game);
    let state = game.world().state().unwrap();
    let attempts = [
        json!({"op": "set_prop", "entity": entry, "key": "summary", "value": "lies"}),
        json!({"op": "remove_prop", "entity": entry, "key": "_entry"}),
        json!({"op": "rename", "entity": entry, "name": "Lies"}),
        json!({"op": "destroy", "entity": entry}),
        json!({"op": "move", "entity": entry, "to": room}),
        json!({"op": "relate", "subject": room, "rel": "near", "object": entry}),
        json!({"op": "relate", "subject": entry, "rel": "near", "object": room}),
        json!({"op": "create", "name": "note", "kind": "note", "location": entry}),
    ];
    for op in attempts {
        let patch: Patch = serde_json::from_value(json!({ "ops": [op] })).unwrap();
        let err = game.world_mut().apply_patch(&patch).unwrap_err();
        assert!(
            matches!(err, ApplyError::Rejected(_)),
            "{op} should be rejected"
        );
    }
    assert_eq!(game.world().state().unwrap(), state);

    // The seal is the engine's `_entry` prop, not the kind: a thing the
    // director calls "knowledge" (as an older save might hold) stays an
    // ordinary entity, and no entry.
    let made = game
        .world_mut()
        .apply_patch(
            &serde_json::from_value(json!({"ops": [
                {"op": "create", "handle": "k", "name": "scroll", "kind": "knowledge", "location": room}
            ]}))
            .unwrap(),
        )
        .unwrap()
        .created[0];
    game.world_mut()
        .apply_patch(&Patch::new(vec![Op::Rename {
            entity: made.into(),
            name: "old scroll".into(),
        }]))
        .unwrap();
    assert_eq!(names(game.world()), ["Drowned Chapel", "Bell Tower"]);
}

#[test]
fn a_turn_without_narration_records_no_encounter() {
    // The director builds a room, then the turn fails before narrating.
    let (mut game, client) = begun();
    client.push(tower_room());
    assert!(game.turn(&client, "go north", &mut |_| {}).is_err());
    assert_ne!(
        game.world()
            .entity(player_room(&game))
            .unwrap()
            .unwrap()
            .name,
        "Drowned Chapel",
        "the move itself happened"
    );
    assert_eq!(names(game.world()), ["Drowned Chapel"]);

    // A turn that moves the player but narrates nothing records nothing.
    let (mut game, client) = begun();
    client.push(tower_room());
    client.push(Response::canned_text(""));
    game.turn(&client, "go north", &mut |_| {}).unwrap();
    assert_eq!(names(game.world()), ["Drowned Chapel"]);
}

#[test]
fn the_writer_refuses_sources_that_are_not_player_visible_narration() {
    let (mut game, _) = in_tower();
    let room = player_room(&game);
    let world = game.world_mut();
    let input = world.last_event(EventKind::Input).unwrap().unwrap().seq;
    let empty = world
        .log_event(EventKind::Narration, &json!({"text": "  "}))
        .unwrap();
    let narration = world.last_event(EventKind::Narration).unwrap().unwrap().seq;
    let before = world.last_seq().unwrap();
    for bad in [input, empty, before + 100] {
        assert!(
            record_place(world, room, None, bad).unwrap().is_err(),
            "#{bad}"
        );
    }
    assert_eq!(world.last_seq().unwrap(), before, "nothing was written");
    // A non-room is refused too.
    let player = world.entities_of_kind(crate::world::PLAYER_KIND).unwrap()[0];
    assert!(
        record_place(world, player, None, narration - 1)
            .unwrap()
            .is_err()
    );
}

#[test]
fn discoveries_survive_reopen_replay_fork_and_an_adopted_reroll() {
    use crate::branches::tests::{saved_game, temp_dir};
    let dir = temp_dir("knowledge-persist");
    let path = saved_game(&dir, "mill.db");
    let mut game = Game::new(World::open(&path).unwrap());
    let client = ScriptedClient::new([tower_room(), Response::canned_text("The tower.")]);
    game.turn(&client, "go north", &mut |_| {}).unwrap();
    let want = entries(game.world()).unwrap();
    assert_eq!(want.len(), 2);
    let last = game.world().last_seq().unwrap();
    drop(game);

    let reopened = World::open(&path).unwrap();
    assert_eq!(entries(&reopened).unwrap(), want);
    assert_eq!(
        entries(&reopened.replay_in_memory().unwrap()).unwrap(),
        want
    );
    let fork = reopened.fork(last, dir.join("fork.db")).unwrap();
    assert_eq!(entries(&fork).unwrap(), want);

    // Reroll the move; the adopted variant remembers its own encounter.
    let game = Game::new(reopened);
    let turn = crate::xray::latest_turn(game.world()).unwrap().unwrap();
    let scripts = [ScriptedClient::new([
        Response::canned_tool_use(
            "r2",
            "create_room",
            json!({"via_exit": "north", "back_exit": "south", "name": "Reed Path", "region": "Saltmarsh",
                   "description": "Reeds close overhead.", "exits": {}, "contents": []}),
        ),
        Response::canned_text("Reeds."),
    ])];
    let spec = crate::reroll::Spec {
        turn: Some(turn),
        n: 1,
        ..crate::reroll::Spec::default()
    };
    let r = crate::reroll::run(
        game.world(),
        &spec,
        game.brains(),
        &|n| &scripts[n - 1] as &(dyn crate::llm::LlmClient + Sync),
        &mut |_| {},
    )
    .unwrap();
    let adopted = dir.join("adopted.db");
    r.adopt(1, &adopted).unwrap();
    let branch = World::open(&adopted).unwrap();
    assert_eq!(names(&branch), ["Drowned Chapel", "Reed Path"]);
    assert_eq!(
        entries(&branch.replay_in_memory().unwrap()).unwrap(),
        entries(&branch).unwrap()
    );
    // The save being played is untouched.
    assert_eq!(entries(game.world()).unwrap(), want);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_save_without_discoveries_reads_as_an_empty_compendium_and_infers_nothing() {
    // A save from before the compendium: forked just after the opening
    // narration, before its encounter was recorded.
    let (game, _) = begun();
    let opening = game
        .world()
        .last_event(EventKind::Narration)
        .unwrap()
        .unwrap()
        .seq;
    let old = game.world().fork_in_memory(opening).unwrap();
    assert!(entries(&old).unwrap().is_empty());
    assert!(compendium(&old, "").contains("empty"));
    assert!(compendium(&old, "places").contains("No places known yet"));
    assert!(compendium(&old, "Drowned Chapel").contains("nothing called"));

    // Playing on in the same room still infers nothing about the past.
    let mut game = Game::new(old);
    let client = ScriptedClient::new([Response::canned_text("You wait.")]);
    game.turn(&client, "wait", &mut |_| {}).unwrap();
    assert!(entries(game.world()).unwrap().is_empty());
    // Entering a room is a new discovery.
    client.push(tower_room());
    client.push(Response::canned_text("The tower."));
    game.turn(&client, "go north", &mut |_| {}).unwrap();
    assert_eq!(names(game.world()), ["Bell Tower"]);
    // The chapel isn't known, so the tower wasn't "reached from" it.
    assert!(place(game.world(), "Bell Tower").facts.is_empty());
}

#[test]
fn reading_the_compendium_makes_no_model_call_and_writes_nothing() {
    let (game, client) = in_tower();
    let world = game.world();
    let calls = client.requests().len();
    let seq = world.last_seq().unwrap();
    let state = world.state().unwrap();
    for arg in [
        "",
        "places",
        "people",
        "Bell Tower",
        "#1",
        "search tower",
        "search",
        "nowhere",
    ] {
        compendium(world, arg);
    }
    entries(world).unwrap();
    search(world, "tower").unwrap();
    find(world, "tower").unwrap();
    assert_eq!(client.requests().len(), calls);
    assert_eq!(world.last_seq().unwrap(), seq);
    assert_eq!(world.state().unwrap(), state);
}

#[test]
fn why_and_relations_do_not_match_compendium_entries() {
    let (game, _) = in_tower();
    let why = commands::report(game.world(), ":why", "Bell Tower")
        .unwrap()
        .unwrap();
    assert!(!why.contains("several"), "{why}");
}

#[test]
fn a_turn_through_several_rooms_claims_no_false_neighbour() {
    // Chapel → Bell Tower → Loft in one turn: the narration ends in the
    // loft, and the chapel has no exit to it.
    let (mut game, client) = begun();
    client.push(tower_room());
    client.push(Response::canned_tool_use(
        "r3",
        "create_room",
        json!({"via_exit": "up", "back_exit": "down", "name": "Loft", "region": "Saltmarsh",
               "description": "Dusty.", "exits": {}, "contents": []}),
    ));
    client.push(Response::canned_text(
        "You climb through the tower to the loft.",
    ));
    game.turn(&client, "climb to the top", &mut |_| {}).unwrap();
    let world = game.world();
    assert_eq!(names(world), ["Drowned Chapel", "Loft"]);
    assert!(place(world, "Loft").facts.is_empty());
}

#[test]
fn a_turn_logs_its_time_right_after_its_narration_then_remembers() {
    // The X-ray reads a turn's time, verdicts and rule changes straight
    // after its narration; the encounter is recorded after them.
    let (game, _) = in_tower();
    let events = game.world().events().unwrap();
    let narration = events
        .iter()
        .rposition(|e| e.kind == EventKind::Narration)
        .unwrap();
    let after: Vec<EventKind> = events[narration + 1..].iter().map(|e| e.kind).collect();
    assert_eq!(after.first(), Some(&EventKind::Patch), "the clock's patch");
    assert!(after.contains(&EventKind::Time), "{after:?}");
    let last = events.last().unwrap();
    assert_eq!(last.kind, EventKind::Patch);
    assert_eq!(last.payload["by"], json!("engine"));
    assert_eq!(
        place(game.world(), "Bell Tower").first_seen,
        events[narration].seq
    );
}

#[test]
fn compendium_entries_stay_out_of_stats_rerolls_and_director_lookups() {
    let (game, _) = in_tower();
    let world = game.world();
    let stats = commands::report(world, ":stats", "").unwrap().unwrap();
    assert!(!stats.contains("knowledge"), "{stats}");
    let before = world.fork_in_memory(1).unwrap().state().unwrap();
    let facts = crate::reroll::facts(world, 1, &before).unwrap();
    assert!(
        facts.iter().any(|f| f.text.contains("Bell Tower (room)")),
        "{facts:?}"
    );
    assert!(
        !facts.iter().any(|f| f.text.contains("(knowledge)")),
        "{facts:?}"
    );

    // The director can't read an entry as if it were a world entity.
    let (mut game, client) = in_tower();
    let entry = place(game.world(), "Bell Tower").id;
    client.push(Response::canned_tool_use(
        "q",
        "query_entity",
        json!({ "id": entry }),
    ));
    client.push(Response::canned_text("You look about."));
    game.turn(&client, "look", &mut |_| {}).unwrap();
    let reply = client
        .requests()
        .last()
        .unwrap()
        .messages
        .last()
        .unwrap()
        .content[0]
        .clone();
    match reply {
        crate::llm::ContentBlock::ToolResult {
            content, is_error, ..
        } => assert!(
            is_error && content.contains("compendium entry"),
            "{content}"
        ),
        other => panic!("expected a tool result, got {other:?}"),
    }
}
