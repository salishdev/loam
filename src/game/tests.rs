use serde_json::{Value, json};

use super::*;
use crate::llm::{ContentBlock, Response, ScriptedClient};

const SECRET: &str = "The abbot drowned the bell to hide the ledger";

fn bible_call() -> Response {
    crate::stats::with_sample_sheet(Response::canned_tool_use(
        "b1",
        "write_bible",
        json!({
            "tone": "Damp, melancholy folk horror.",
            "regions": [{"name": "Saltmarsh", "description": "Reed beds around a sunken abbey."}],
            "factions": [{"name": "Eel-wives", "description": "Keep the old tithes."}],
            "npcs": [{"name": "Brother Ash", "description": "Last monk, lies kindly."}],
            "secrets": [{"name": "The bell", "description": SECRET}]
        }),
    ))
}

fn opening_room() -> Response {
    Response::canned_tool_use(
        "r1",
        "create_room",
        json!({
            "name": "Drowned Chapel",
            "region": "Saltmarsh",
            "description": "Brackish water laps at the pews.",
            "exits": {"north": "a reed path toward the bell tower"},
            "contents": [
                {"handle": "altar", "name": "altar", "kind": "feature", "props": {"material": "slate"}},
                {"name": "tallow candle", "kind": "item", "inside": "altar", "props": {"lit": false}}
            ]
        }),
    )
}

fn new_game() -> (Game, ScriptedClient) {
    let client = ScriptedClient::new([bible_call(), Response::canned_text("")]);
    let mut game = Game::new(World::open_in_memory().unwrap());
    game.set_dice_seed(7);
    game.create_bible(&client, "A drowned abbey in the fens.")
        .unwrap();
    (game, client)
}

/// A started game: bible written and the opening room generated.
fn begun() -> (Game, ScriptedClient) {
    let (mut game, client) = new_game();
    client.push(Response::canned_tool_use(
        "t0",
        "read_bible",
        json!({"section": "regions"}),
    ));
    client.push(opening_room());
    client.push(Response::canned_text("You wake in a flooded chapel."));
    game.begin(&client, &mut |_| {}).unwrap();
    (game, client)
}

fn last_tool_result(client: &ScriptedClient) -> (String, bool) {
    let req = client.requests().last().cloned().unwrap();
    match req.messages.last().unwrap().content.first().unwrap() {
        ContentBlock::ToolResult {
            content, is_error, ..
        } => (content.clone(), *is_error),
        other => panic!("expected a tool result, got {other:?}"),
    }
}

fn first_user_text(req: &crate::llm::Request) -> String {
    match &req.messages[0].content[0] {
        ContentBlock::Text { text } => text.clone(),
        other => panic!("expected text, got {other:?}"),
    }
}

fn player_room(game: &Game) -> i64 {
    let p = player_id(game.world()).unwrap().unwrap();
    game.world().entity(p).unwrap().unwrap().location.unwrap()
}

#[test]
fn seed_builds_a_bible_readable_through_tools() {
    let (game, client) = new_game();
    assert!(game.has_bible().unwrap());
    assert!(!game.has_begun().unwrap());
    let bible = game
        .world()
        .entity(bible_id(game.world()).unwrap().unwrap())
        .unwrap()
        .unwrap();
    let mut keys: Vec<_> = bible.props.keys().cloned().collect();
    keys.sort();
    let mut want = BIBLE_SECTIONS.to_vec();
    want.sort();
    assert_eq!(keys, want);
    // The bible call offers write_bible and write_stats, and the seed is
    // the prompt.
    let req = &client.requests()[0];
    assert_eq!(req.model, Model::Sonnet);
    assert_eq!(first_user_text(req), "A drowned abbey in the fens.");
    assert_eq!(
        req.tools
            .iter()
            .map(|t| t.name.as_str())
            .collect::<Vec<_>>(),
        ["write_bible", "write_stats"]
    );
    assert_eq!(game.stats().unwrap().len(), 2);

    let mut game = game;
    let client = ScriptedClient::new([
        Response::canned_tool_use("x", "read_bible", json!({"section": "secrets"})),
        Response::canned_text("ok"),
    ]);
    // read_bible works before any room exists.
    game.begin(&client, &mut |_| {}).unwrap();
    let (content, is_error) = last_tool_result(&client);
    assert!(!is_error);
    assert!(content.contains(SECRET));
}

#[test]
fn missing_bible_is_an_error() {
    let client = ScriptedClient::new([Response::canned_text("I'd rather not.")]);
    let mut game = Game::new(World::open_in_memory().unwrap());
    assert!(matches!(
        game.create_bible(&client, "seed"),
        Err(GameError::NoBible)
    ));
    let client = ScriptedClient::default();
    assert!(matches!(
        game.turn(&client, "look", &mut |_| {}),
        Err(GameError::NotStarted)
    ));
}

#[test]
fn incomplete_bible_is_refused_so_the_director_can_retry() {
    let client = ScriptedClient::new([
        Response::canned_tool_use(
            "b0",
            "write_bible",
            json!({"tone": "grim", "regions": [], "factions": [], "npcs": [], "secrets": []}),
        ),
        bible_call(),
        Response::canned_text(""),
    ]);
    let mut game = Game::new(World::open_in_memory().unwrap());
    game.create_bible(&client, "seed").unwrap();
    let retry = &client.requests()[1];
    match &retry.messages[2].content[0] {
        ContentBlock::ToolResult {
            is_error, content, ..
        } => {
            assert!(is_error);
            assert!(content.contains("regions"), "{content}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn context_holds_section_titles_but_not_bible_contents() {
    let (game, client) = begun();
    let opening = first_user_text(&client.requests()[2]);
    assert!(
        opening.contains("Bible sections (read_bible): factions, npcs, regions, secrets, tone")
    );
    assert!(opening.contains("Location: nowhere yet"));
    assert!(!opening.contains(SECRET));
    assert!(!opening.contains("Reed beds"));
    let slice = local_slice(game.world()).unwrap();
    assert!(!slice.contains(SECRET));
    assert!(slice.contains("Location: #"));
    assert!(slice.contains("Drowned Chapel"));
    // Contents one level down, with props.
    assert!(
        slice.contains("altar [feature] {\"material\":\"slate\"}"),
        "{slice}"
    );
    assert!(
        slice.contains("  - #"),
        "candle should show inside the altar: {slice}"
    );
    assert!(slice.contains("north -> unexplored (a reed path toward the bell tower)"));
}

#[test]
fn opening_generates_the_first_room_and_narrates() {
    let (game, client) = begun();
    assert!(game.has_begun().unwrap());
    let room = game.world().entity(player_room(&game)).unwrap().unwrap();
    assert_eq!(room.kind, ROOM_KIND);
    assert_eq!(room.props["region"], json!("Saltmarsh"));
    let events = game.world().events().unwrap();
    let last = events.last().unwrap();
    assert_eq!(last.kind, EventKind::Narration);
    assert_eq!(last.payload["text"], json!("You wake in a flooded chapel."));
    // Tool calls are logged.
    assert!(events.iter().any(|e| e.kind == EventKind::ToolCall
        && e.payload["name"] == json!("create_room")
        && e.payload["is_error"] == json!(false)));
    assert_eq!(client.remaining(), 0);
}

#[test]
fn turn_streams_narration_and_logs_input() {
    let (mut game, client) = begun();
    client.push(Response::canned_text(
        "The candle gutters as you breathe on it.",
    ));
    let mut streamed = String::new();
    let outcome = game
        .turn(&client, "blow on the candle", &mut |t| streamed.push_str(t))
        .unwrap();
    assert_eq!(streamed, "The candle gutters as you breathe on it.");
    assert_eq!(outcome.narration, streamed);
    assert_eq!(outcome.stop_reason, None);
    let text = first_user_text(client.requests().last().unwrap());
    assert!(text.ends_with("Player input: blow on the candle"));
    // The previous turn's narration is in Recent; this input is not repeated there.
    assert!(
        text.contains("Recent:\nYou wake in a flooded chapel."),
        "{text}"
    );
    let inputs: Vec<_> = game
        .world()
        .events()
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EventKind::Input)
        .map(|e| e.payload["text"].clone())
        .collect();
    assert_eq!(inputs.last().unwrap(), &json!("blow on the candle"));
}

#[test]
fn rooms_are_generated_on_arrival_and_the_same_on_return() {
    let (mut game, client) = begun();
    let chapel = player_room(&game);
    let chapel_before = local_slice(game.world()).unwrap();
    let here =
        |s: &str| s[s.find("Location:").unwrap()..s.find("Recent:").unwrap_or(s.len())].to_owned();

    // Unexplored exits can't be walked through.
    client.push(Response::canned_tool_use(
        "m0",
        "move_player",
        json!({"exit": "north"}),
    ));
    client.push(Response::canned_text("x"));
    game.turn(&client, "go north", &mut |_| {}).unwrap();
    let (msg, is_error) = last_tool_result(&client);
    assert!(is_error && msg.contains("create_room"), "{msg}");
    assert_eq!(player_room(&game), chapel);

    client.push(Response::canned_tool_use(
        "r2",
        "create_room",
        json!({
            "via_exit": "north",
            "back_exit": "south",
            "name": "Bell Tower",
            "region": "Saltmarsh",
            "description": "A leaning tower; the bell is gone.",
            "exits": {"up": "rotten stairs"},
            "contents": [{"name": "crow", "kind": "npc", "props": {"mood": "wary"}}]
        }),
    ));
    client.push(Response::canned_text("You climb to the tower."));
    game.turn(&client, "go north", &mut |_| {}).unwrap();
    let tower = player_room(&game);
    assert_ne!(tower, chapel);
    let tower_slice = local_slice(game.world()).unwrap();
    assert!(tower_slice.contains(&format!("south -> #{chapel} Drowned Chapel")));
    assert!(tower_slice.contains("crow [npc]"));

    // Exploring the same exit twice is refused.
    client.push(Response::canned_tool_use(
        "r3",
        "create_room",
        json!({"via_exit": "south", "back_exit": "north", "name": "Elsewhere", "region": "Saltmarsh",
               "description": "", "exits": {}, "contents": []}),
    ));
    client.push(Response::canned_text("x"));
    game.turn(&client, "go south", &mut |_| {}).unwrap();
    let (msg, is_error) = last_tool_result(&client);
    assert!(is_error && msg.contains("already explored"), "{msg}");

    client.push(Response::canned_tool_use(
        "m1",
        "move_player",
        json!({"exit": "south"}),
    ));
    client.push(Response::canned_text("Back in the chapel."));
    game.turn(&client, "go back south", &mut |_| {}).unwrap();
    assert_eq!(player_room(&game), chapel);
    let chapel_after = local_slice(game.world()).unwrap();
    let mut expected = here(&chapel_before);
    expected = expected.replace(
        "north -> unexplored (a reed path toward the bell tower)",
        &format!("north -> #{tower} Bell Tower"),
    );
    assert_eq!(here(&chapel_after), expected);
    // On the next turn the director sees the same room it left.
    client.push(Response::canned_text("Water laps at the pews."));
    game.turn(&client, "look", &mut |_| {}).unwrap();
    let seen = first_user_text(client.requests().last().unwrap());
    assert!(seen.contains(&expected), "{seen}");

    // No room was generated twice.
    assert_eq!(
        game.world().entities_of_kind(ROOM_KIND).unwrap(),
        vec![chapel, tower]
    );
}

#[test]
fn rooms_persist_across_reopening_the_save() {
    let dir = std::env::temp_dir().join(format!("loam-game-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("save.db");
    let _ = std::fs::remove_file(&path);

    let client = ScriptedClient::new([
        bible_call(),
        Response::canned_text(""),
        opening_room(),
        Response::canned_text("Opening."),
    ]);
    let mut game = Game::new(World::open(&path).unwrap());
    game.create_bible(&client, "seed").unwrap();
    game.begin(&client, &mut |_| {}).unwrap();
    let before = local_slice(game.world()).unwrap();
    drop(game);

    let game = Game::new(World::open(&path).unwrap());
    assert!(game.has_begun().unwrap());
    assert_eq!(local_slice(game.world()).unwrap(), before);
    assert!(game.look().unwrap().starts_with("Drowned Chapel\n"));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn patches_change_the_world_and_rejections_reach_the_director() {
    let (mut game, client) = begun();
    let candle = game
        .world()
        .state()
        .unwrap()
        .entities
        .into_iter()
        .find(|e| e.name == "tallow candle")
        .unwrap()
        .id;
    client.push(Response::canned_tool_use(
        "p1",
        "apply_patch",
        json!({"ops": [{"op": "set_prop", "entity": candle, "key": "lit", "value": true}]}),
    ));
    client.push(Response::canned_tool_use(
        "p2",
        "apply_patch",
        json!({"ops": [{"op": "move", "entity": 999, "to": candle}]}),
    ));
    client.push(Response::canned_text("The candle catches."));
    game.turn(&client, "light the candle", &mut |_| {}).unwrap();
    assert_eq!(
        game.world().entity(candle).unwrap().unwrap().props["lit"],
        json!(true)
    );
    let (msg, is_error) = last_tool_result(&client);
    assert!(is_error && msg.contains("#999 does not exist"), "{msg}");
}

#[test]
fn bible_and_player_are_protected_from_patches() {
    let (mut game, client) = begun();
    let bible = bible_id(game.world()).unwrap().unwrap();
    let player = player_id(game.world()).unwrap().unwrap();
    for (i, ops) in [
        json!([{"op": "set_prop", "entity": bible, "key": "tone", "value": "silly"}]),
        json!([{"op": "destroy", "entity": player}]),
        json!([{"op": "move", "entity": bible, "to": player_room(&game)}]),
        json!([{"op": "create", "name": "note", "kind": "item", "location": bible}]),
    ]
    .into_iter()
    .enumerate()
    {
        client.push(Response::canned_tool_use(
            format!("p{i}"),
            "apply_patch",
            json!({"ops": ops}),
        ));
        client.push(Response::canned_text("x"));
        game.turn(&client, "cheat", &mut |_| {}).unwrap();
        let (_, is_error) = last_tool_result(&client);
        assert!(is_error);
    }
    let tone = &game.world().entity(bible).unwrap().unwrap().props["tone"];
    assert_eq!(tone, &Value::from("Damp, melancholy folk horror."));
}

#[test]
fn query_and_search_tools_answer_from_the_store() {
    let (mut game, client) = begun();
    let chapel = player_room(&game);
    client.push(Response::canned_tool_use(
        "q",
        "query_entity",
        json!({"id": chapel}),
    ));
    client.push(Response::canned_text("x"));
    game.turn(&client, "look", &mut |_| {}).unwrap();
    let (msg, is_error) = last_tool_result(&client);
    assert!(!is_error);
    let v: Value = serde_json::from_str(&msg).unwrap();
    assert_eq!(v["name"], json!("Drowned Chapel"));
    assert!(
        v["contents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["name"] == json!("altar"))
    );

    client.push(Response::canned_tool_use(
        "s",
        "search_events",
        json!({"text": "FLOODED", "kinds": ["narration"]}),
    ));
    client.push(Response::canned_text("x"));
    game.turn(&client, "remember", &mut |_| {}).unwrap();
    let (msg, is_error) = last_tool_result(&client);
    assert!(!is_error);
    let v: Value = serde_json::from_str(&msg).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 1);
    assert_eq!(
        v[0]["payload"]["text"],
        json!("You wake in a flooded chapel.")
    );
}

#[test]
fn bible_tool_is_not_offered_during_play() {
    let (mut game, client) = begun();
    client.push(bible_call());
    client.push(Response::canned_text("x"));
    game.turn(&client, "rewrite the world", &mut |_| {})
        .unwrap();
    let (msg, is_error) = last_tool_result(&client);
    assert!(is_error && msg.contains("no tool named"), "{msg}");
}

#[test]
fn a_played_session_replays_to_the_same_state() {
    let (mut game, client) = begun();
    client.push(Response::canned_tool_use(
        "r2",
        "create_room",
        json!({"via_exit": "north", "back_exit": "south", "name": "Bell Tower", "region": "Saltmarsh",
               "description": "Tall.", "exits": {}, "contents": [{"name": "rope", "kind": "item"}]}),
    ));
    client.push(Response::canned_text("Up you go."));
    game.turn(&client, "north", &mut |_| {}).unwrap();
    let replayed = game.world().replay_in_memory().unwrap();
    assert_eq!(replayed.state().unwrap(), game.world().state().unwrap());
    assert_eq!(
        local_slice(&replayed).unwrap(),
        local_slice(game.world()).unwrap()
    );
}

#[test]
fn the_player_carries_items_through_the_inventory_tools() {
    let (mut game, client) = begun();
    let candle = game
        .world()
        .state()
        .unwrap()
        .entities
        .into_iter()
        .find(|e| e.name == "tallow candle")
        .unwrap();
    // Moving it straight into the player's inventory is refused...
    let player = player_id(game.world()).unwrap().unwrap();
    client.push(Response::canned_tool_use(
        "p",
        "apply_patch",
        json!({"ops": [{"op": "move", "entity": candle.id, "to": player}]}),
    ));
    client.push(Response::canned_text("x"));
    game.turn(&client, "grab the candle", &mut |_| {}).unwrap();
    assert!(last_tool_result(&client).1);
    // ...the inventory tool does it. The candle sits in the altar, which is open.
    client.push(Response::canned_tool_use(
        "t",
        "take",
        json!({"item": candle.id}),
    ));
    client.push(Response::canned_text("You take the candle."));
    game.turn(&client, "take the candle", &mut |_| {}).unwrap();
    let (msg, is_error) = last_tool_result(&client);
    assert!(!is_error, "{msg}");
    let slice = local_slice(game.world()).unwrap();
    assert!(
        slice.contains(&format!("Carrying:\n- #{} tallow candle", candle.id)),
        "{slice}"
    );
    let replayed = game.world().replay_in_memory().unwrap();
    assert_eq!(replayed.state().unwrap(), game.world().state().unwrap());
}

// ---- rules, glossary and the fast path --------------------------------------

/// A begun game with two rotten doors in the opening room.
fn with_doors() -> (Game, ScriptedClient, i64, i64) {
    let (mut game, client) = begun();
    let room = player_room(&game);
    let ops = serde_json::from_value(json!([
        {"op": "create", "handle": "oak", "name": "oak door", "kind": "door", "location": room, "props": {"rotten": true}},
        {"op": "create", "handle": "elm", "name": "elm door", "kind": "door", "location": room, "props": {"rotten": true}}
    ]))
    .unwrap();
    let a = game
        .world
        .apply_patch(&crate::world::Patch::new(ops))
        .unwrap();
    (game, client, a.handles["oak"], a.handles["elm"])
}

fn kick_mint(target: i64) -> Response {
    Response::canned_tool_use(
        "k1",
        "mint_rule",
        json!({
            "name": "Kicking a rotten door",
            "trigger": "kick, shoulder or force <target>",
            "roles": {"target": "the door"},
            "conditions": [
                {"check": "kind", "role": "target", "is": "door"},
                {"check": "prop", "role": "target", "prop": "rotten", "is": true},
                {"check": "not_prop", "role": "target", "prop": "broken", "is": true}
            ],
            "effects": [
                {"op": "set_prop", "entity": "target", "key": "broken", "value": true},
                {"op": "create", "name": "splinters", "kind": "debris", "location": "room", "props": {"state": "damp"}}
            ],
            "outcome": "The door bursts inward in wet splinters.",
            "apply_to": {"target": target}
        }),
    )
}

fn interpret(matches: Value, clean: bool) -> Response {
    Response::canned_tool_use(
        "i",
        "interpret",
        json!({"matches": matches, "clean": clean}),
    )
}

fn models_since(client: &ScriptedClient, from: usize) -> Vec<(Model, usize)> {
    client.requests()[from..]
        .iter()
        .map(|r| (r.model.clone(), r.tools.len()))
        .collect()
}

/// Props and new entities a turn produced, with ids abstracted away.
fn change_shape(
    game: &Game,
    door: i64,
    before: &crate::world::WorldState,
) -> (Value, Vec<(String, String, Value)>) {
    let after = game.world().state().unwrap();
    let props = Value::Object(game.world().entity(door).unwrap().unwrap().props);
    let new = after
        .entities
        .iter()
        .filter(|e| !before.entities.iter().any(|b| b.id == e.id))
        .filter(|e| e.kind != crate::rules::RULE_KIND && e.kind != crate::glossary::GLOSSARY_KIND)
        .map(|e| {
            (
                e.name.clone(),
                e.kind.clone(),
                Value::Object(e.props.clone()),
            )
        })
        .collect();
    (props, new)
}

#[test]
fn a_ruled_action_repeats_on_the_fast_path_with_the_same_change() {
    let (mut game, client, oak, elm) = with_doors();

    // Novel: no rules yet, so no interpreter call; the director rules and mints.
    let before = game.world().state().unwrap();
    let start = client.requests().len();
    client.push(kick_mint(oak));
    client.push(Response::canned_text("The oak door bursts."));
    let first = game
        .turn(&client, "kick the oak door", &mut |_| {})
        .unwrap();
    assert_eq!(first.fast_path, None);
    assert_eq!(models_since(&client, start)[0].0, Model::Sonnet);
    let (msg, is_error) = last_tool_result(&client);
    assert!(!is_error, "{msg}");
    let minted: Value = serde_json::from_str(&msg).unwrap();
    let rule = minted["rule"].as_i64().unwrap();
    assert!(
        minted["new_glossary_terms"]
            .as_array()
            .unwrap()
            .contains(&json!("broken"))
    );
    let first_change = change_shape(&game, oak, &before);
    assert_eq!(first_change.0["broken"], json!(true));

    // Repeat on a matching entity: Haiku matches, engine applies, narrator narrates.
    let before = game.world().state().unwrap();
    let start = client.requests().len();
    client.push(interpret(
        json!([{"rule": rule, "bindings": {"target": elm}}]),
        true,
    ));
    client.push(Response::canned_text("The elm door gives way too."));
    let mut streamed = String::new();
    let second = game
        .turn(&client, "boot the elm door", &mut |t| streamed.push_str(t))
        .unwrap();
    assert_eq!(second.fast_path, Some(rule));
    assert_eq!(second.calls, 2);
    assert_eq!(streamed, "The elm door gives way too.");
    // Haiku with only the interpret tool, then a tool-less narrator: no director.
    assert_eq!(
        models_since(&client, start),
        [(Model::Haiku, 1), (Model::Sonnet, 0)]
    );
    let reqs = client.requests();
    let interp = first_user_text(&reqs[start]);
    assert!(
        interp.contains("Kicking a rotten door")
            && interp.ends_with("Player input: boot the elm door")
    );
    let narr = first_user_text(&reqs[start + 1]);
    assert!(
        narr.contains("The door bursts inward in wet splinters."),
        "{narr}"
    );
    assert!(narr.contains("Damp, melancholy folk horror."), "{narr}");
    assert_eq!(change_shape(&game, elm, &before), first_change);

    // The log explains it: a judgment, a patch tagged with the rule, the narration.
    let events = game.world().events().unwrap();
    let tail: Vec<_> = events.iter().rev().take(4).rev().collect();
    assert_eq!(
        tail.iter().map(|e| e.kind).collect::<Vec<_>>(),
        [
            EventKind::Input,
            EventKind::Judgment,
            EventKind::Patch,
            EventKind::Narration
        ]
    );
    assert_eq!(tail[1].payload["verdict"], json!("fast_path"));
    assert_eq!(tail[2].payload["rule"], json!({"id": rule, "version": 1}));

    // The same door again: the rule matches but its conditions fail, so the
    // director decides, told why.
    client.push(interpret(
        json!([{"rule": rule, "bindings": {"target": oak}}]),
        true,
    ));
    client.push(Response::canned_text("It is already in pieces."));
    let third = game
        .turn(&client, "kick the oak door", &mut |_| {})
        .unwrap();
    assert_eq!(third.fast_path, None);
    let director = first_user_text(client.requests().last().unwrap());
    assert!(
        director.contains("Engine note: Rules matched this input but don't apply here")
            && director.contains("target.broken should not be true, is true"),
        "{director}"
    );

    let replayed = game.world().replay_in_memory().unwrap();
    assert_eq!(replayed.state().unwrap(), game.world().state().unwrap());
}

#[test]
fn a_rule_not_meeting_its_conditions_is_not_minted() {
    let (mut game, client, oak, _) = with_doors();
    game.world
        .apply_patch(&crate::world::Patch::new(vec![crate::world::Op::SetProp {
            entity: oak.into(),
            key: "rotten".into(),
            value: json!(false),
        }]))
        .unwrap();
    client.push(kick_mint(oak));
    client.push(Response::canned_text("x"));
    game.turn(&client, "kick the oak door", &mut |_| {})
        .unwrap();
    let (msg, is_error) = last_tool_result(&client);
    assert!(is_error && msg.contains("rule not minted"), "{msg}");
    assert!(crate::rules::active_world(game.world()).unwrap().is_empty());
}

#[test]
fn unclear_interpretations_go_to_the_director() {
    let (mut game, client, oak, elm) = with_doors();
    client.push(kick_mint(oak));
    client.push(Response::canned_text("x"));
    game.turn(&client, "kick the oak door", &mut |_| {})
        .unwrap();
    let rule = crate::rules::active_world(game.world()).unwrap()[0].id;

    // More than the rule covers.
    client.push(interpret(
        json!([{"rule": rule, "bindings": {"target": elm}}]),
        false,
    ));
    client.push(Response::canned_text("x"));
    let t = game
        .turn(&client, "kick the elm door and run", &mut |_| {})
        .unwrap();
    assert_eq!(t.fast_path, None);
    assert!(
        first_user_text(client.requests().last().unwrap()).contains("covers part of this input")
    );
    assert!(
        !game
            .world()
            .entity(elm)
            .unwrap()
            .unwrap()
            .props
            .contains_key("broken")
    );

    // No tool call, or no match: a plain director turn.
    for reply in [Response::canned_text("no idea"), interpret(json!([]), true)] {
        client.push(reply);
        client.push(Response::canned_text("x"));
        let t = game.turn(&client, "sing to the crow", &mut |_| {}).unwrap();
        assert_eq!(t.fast_path, None);
        let text = first_user_text(client.requests().last().unwrap());
        assert!(!text.contains("Engine note"), "{text}");
        assert!(text.ends_with("Player input: sing to the crow"));
    }
}

#[test]
fn conflicting_rules_escalate_and_the_director_can_retire_one() {
    let (mut game, client, oak, elm) = with_doors();
    client.push(kick_mint(oak));
    client.push(Response::canned_text("x"));
    game.turn(&client, "kick the oak door", &mut |_| {})
        .unwrap();
    let mut second = kick_mint(elm);
    if let ContentBlock::ToolUse(t) = &mut second.content[0] {
        t.input["name"] = json!("Doors hold firm");
        t.input["effects"] = json!([]);
        t.input.as_object_mut().unwrap().remove("apply_to");
    }
    client.push(interpret(json!([]), true));
    client.push(second);
    client.push(Response::canned_text("x"));
    game.turn(&client, "think about doors", &mut |_| {})
        .unwrap();
    let ids: Vec<i64> = crate::rules::active_world(game.world())
        .unwrap()
        .iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(ids.len(), 2);

    client.push(interpret(
        json!([{"rule": ids[0], "bindings": {"target": elm}}, {"rule": ids[1], "bindings": {"target": elm}}]),
        true,
    ));
    client.push(Response::canned_tool_use(
        "l",
        "list_rules",
        json!({"text": "door"}),
    ));
    client.push(Response::canned_tool_use(
        "r",
        "retire_rule",
        json!({"id": ids[1], "reason": "contradicts the rotten-door rule"}),
    ));
    client.push(Response::canned_tool_use("l2", "list_rules", json!({})));
    client.push(Response::canned_text("The elm door shudders, undecided."));
    game.turn(&client, "kick the elm door", &mut |_| {})
        .unwrap();
    let reqs = client.requests();
    let director = first_user_text(&reqs[reqs.len() - 4]);
    assert!(director.contains("Engine note: Conflict:"), "{director}");
    assert!(director.contains("Doors hold firm"), "{director}");

    // list_rules before and after the retirement.
    let listed = |i: usize| -> Value {
        match reqs[i].messages.last().unwrap().content.first().unwrap() {
            ContentBlock::ToolResult {
                content, is_error, ..
            } => {
                assert!(!is_error, "{content}");
                serde_json::from_str(content).unwrap()
            }
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(listed(reqs.len() - 3)["rules"].as_array().unwrap().len(), 2);
    let after = listed(reqs.len() - 1);
    assert_eq!(after["rules"].as_array().unwrap().len(), 1);
    assert_eq!(after["rules"][0]["id"], json!(ids[0]));
    let retired = crate::rules::get(game.world(), ids[1]).unwrap().unwrap();
    assert_eq!(retired.version, 2);
    assert!(!retired.is_active());
}

#[test]
fn the_glossary_index_is_in_context_and_new_terms_are_registered() {
    let (mut game, client) = begun();
    let slice = local_slice(game.world()).unwrap();
    // create_room registered the opening room's props.
    assert!(
        slice.contains("Glossary (lookup_terms; ? = undefined): "),
        "{slice}"
    );
    assert!(
        slice.contains("material?") && slice.contains("lit?") && slice.contains("closed,"),
        "{slice}"
    );

    let altar = game
        .world()
        .state()
        .unwrap()
        .entities
        .into_iter()
        .find(|e| e.name == "altar")
        .unwrap()
        .id;
    client.push(Response::canned_tool_use(
        "p",
        "apply_patch",
        json!({"ops": [{"op": "set_prop", "entity": altar, "key": "cracked", "value": true}]}),
    ));
    client.push(Response::canned_tool_use(
        "d",
        "define_terms",
        json!({"terms": [{"name": "cracked", "meaning": "split; may break further", "values": {"true": "visibly split"}}]}),
    ));
    client.push(Response::canned_tool_use(
        "l",
        "lookup_terms",
        json!({"names": ["cracked", "rotten"]}),
    ));
    client.push(Response::canned_text("x"));
    game.turn(&client, "strike the altar", &mut |_| {}).unwrap();
    let reqs = client.requests();
    let result = |i: usize| match reqs[i].messages.last().unwrap().content.first().unwrap() {
        ContentBlock::ToolResult { content, .. } => serde_json::from_str::<Value>(content).unwrap(),
        other => panic!("{other:?}"),
    };
    assert_eq!(
        result(reqs.len() - 3)["new_glossary_terms"],
        json!(["cracked"])
    );
    let looked = result(reqs.len() - 1);
    assert_eq!(
        looked["terms"]["cracked"]["meaning"],
        json!("split; may break further")
    );
    assert_eq!(looked["not_in_glossary"], json!(["rotten"]));
    assert!(!local_slice(game.world()).unwrap().contains("cracked?"));
}

#[test]
fn rules_and_glossary_are_protected_from_patches() {
    let (mut game, client, oak, _) = with_doors();
    client.push(kick_mint(oak));
    client.push(Response::canned_text("x"));
    game.turn(&client, "kick the oak door", &mut |_| {})
        .unwrap();
    let rule = crate::rules::active_world(game.world()).unwrap()[0].id;
    let glossary = crate::glossary::glossary_id(game.world()).unwrap().unwrap();
    for (i, (ops, want)) in [
        (json!([{"op": "destroy", "entity": rule}]), "mint_rule"),
        (
            json!([{"op": "set_prop", "entity": glossary, "key": "x", "value": 1}]),
            "define_terms",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        client.push(interpret(json!([]), true));
        client.push(Response::canned_tool_use(
            format!("p{i}"),
            "apply_patch",
            json!({"ops": ops}),
        ));
        client.push(Response::canned_text("x"));
        game.turn(&client, "cheat", &mut |_| {}).unwrap();
        let (msg, is_error) = last_tool_result(&client);
        assert!(is_error && msg.contains(want), "{msg}");
    }
    assert!(
        crate::rules::get(game.world(), rule)
            .unwrap()
            .unwrap()
            .is_active()
    );
    for (id, name) in [(rule, "list_rules"), (glossary, "lookup_terms")] {
        client.push(interpret(json!([]), true));
        client.push(Response::canned_tool_use(
            "q",
            "query_entity",
            json!({"id": id}),
        ));
        client.push(Response::canned_text("x"));
        game.turn(&client, "peek", &mut |_| {}).unwrap();
        let (msg, is_error) = last_tool_result(&client);
        assert!(is_error && msg.contains(name), "{msg}");
    }
}

#[test]
fn an_interpreter_failure_falls_back_to_the_director() {
    let (mut game, client, oak, _) = with_doors();
    client.push(kick_mint(oak));
    client.push(Response::canned_text("x"));
    game.turn(&client, "kick the oak door", &mut |_| {})
        .unwrap();

    struct FailFirst<'a>(&'a ScriptedClient, std::cell::Cell<bool>);
    impl crate::llm::LlmClient for FailFirst<'_> {
        fn send(
            &self,
            r: &crate::llm::Request,
            on_text: &mut dyn FnMut(&str),
        ) -> Result<Response, crate::llm::LlmError> {
            if self.1.replace(false) {
                return Err(crate::llm::LlmError::Stream("overloaded".into()));
            }
            self.0.send(r, on_text)
        }
    }
    client.push(Response::canned_text("The director carries on."));
    let t = game
        .turn(
            &FailFirst(&client, true.into()),
            "kick the elm door",
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(t.narration, "The director carries on.");
    let judgment = game
        .world()
        .events()
        .unwrap()
        .into_iter()
        .rfind(|e| e.kind == EventKind::Judgment)
        .unwrap();
    assert!(
        judgment.payload["error"]
            .as_str()
            .unwrap()
            .contains("overloaded")
    );
}

#[test]
fn the_quest_judge_also_rules_on_fast_path_turns() {
    let (mut game, client, oak, elm) = with_doors();
    client.push(kick_mint(oak));
    client.push(Response::canned_text("x"));
    game.turn(&client, "kick the oak door", &mut |_| {})
        .unwrap();
    let rule = crate::rules::active_world(game.world()).unwrap()[0].id;
    let q = crate::quest::spawn(
        &mut game.world,
        "Through the elm",
        "It bars the way",
        &[elm],
    )
    .unwrap();
    crate::quest::reveal_goal(&mut game.world, q, "Break the elm door", &[elm]).unwrap();

    client.push(interpret(
        json!([{"rule": rule, "bindings": {"target": elm}}]),
        true,
    ));
    client.push(Response::canned_text("The elm door gives way."));
    client.push(Response::canned_tool_use(
        "j",
        crate::quest::JUDGE_TOOL,
        json!({"verdict": "complete", "reason": "the door is broken"}),
    ));
    let t = game
        .turn(&client, "kick the elm door", &mut |_| {})
        .unwrap();
    assert_eq!(t.fast_path, Some(rule));
    assert_eq!(t.judgments.len(), 1);
    assert_eq!(t.judgments[0].quest, q);
    assert_eq!(client.remaining(), 0);
}

// ---- world tick --------------------------------------------------------------

/// Chapel (player) and Bell Tower to its north, with a crow in the tower;
/// optionally a Loft above the tower. Leaves the player back in the chapel.
/// Returns (chapel, tower, crow, loft).
fn two_rooms(with_loft: bool) -> (Game, ScriptedClient, i64, i64, i64, Option<i64>) {
    let (mut game, client) = begun();
    let chapel = player_room(&game);
    client.push(Response::canned_tool_use(
        "r2",
        "create_room",
        json!({"via_exit": "north", "back_exit": "south", "name": "Bell Tower", "region": "Saltmarsh",
               "description": "A leaning tower.", "exits": {"up": "rotten stairs"},
               "contents": [{"name": "crow", "kind": "npc", "props": {"mood": "wary"}}]}),
    ));
    client.push(Response::canned_text("The tower."));
    game.turn(&client, "go north", &mut |_| {}).unwrap();
    let tower = player_room(&game);
    let crow = game.world().entities_of_kind("npc").unwrap()[0];
    let mut loft = None;
    if with_loft {
        client.push(Response::canned_tool_use(
            "r3",
            "create_room",
            json!({"via_exit": "up", "back_exit": "down", "name": "Loft", "region": "Saltmarsh",
                   "description": "Dusty.", "exits": {}, "contents": []}),
        ));
        client.push(Response::canned_text("The loft."));
        game.turn(&client, "go up", &mut |_| {}).unwrap();
        loft = Some(player_room(&game));
        client.push(Response::canned_tool_use(
            "m1",
            "move_player",
            json!({"exit": "down"}),
        ));
        client.push(Response::canned_text("Down."));
        game.turn(&client, "go down", &mut |_| {}).unwrap();
    }
    client.push(Response::canned_tool_use(
        "m2",
        "move_player",
        json!({"exit": "south"}),
    ));
    client.push(Response::canned_text("The chapel again."));
    game.turn(&client, "go south", &mut |_| {}).unwrap();
    assert_eq!(player_room(&game), chapel);
    (game, client, chapel, tower, crow, loft)
}

fn quiet_tick(game: &mut Game, client: &ScriptedClient) -> TickOutcome {
    client.push(Response::canned_text("Nothing stirs."));
    game.tick(client).unwrap()
}

fn look(game: &mut Game, client: &ScriptedClient) {
    client.push(Response::canned_text("You look around."));
    game.turn(client, "look", &mut |_| {}).unwrap();
}

#[test]
fn a_tick_is_due_every_few_turns_in_the_same_room() {
    let (mut game, client) = begun();
    assert!(!game.tick_due().unwrap(), "no turns played yet");
    for _ in 1..TICK_EVERY {
        look(&mut game, &client);
        assert!(!game.tick_due().unwrap());
    }
    look(&mut game, &client);
    assert!(game.tick_due().unwrap());

    // maybe_tick runs it and the count starts again.
    client.push(Response::canned_text("Nothing stirs."));
    let tick = game.maybe_tick(&client).unwrap().expect("tick was due");
    assert_eq!(tick.summary, "Nothing stirs.");
    assert!(tick.changes.is_empty());
    assert!(!game.tick_due().unwrap());
    assert_eq!(game.maybe_tick(&client).unwrap(), None);
    assert_eq!(client.remaining(), 0, "a tick that isn't due calls nothing");
    let ticks = game
        .world()
        .search_events("", &[EventKind::Tick], 10)
        .unwrap();
    assert_eq!(ticks.len(), 1);
    assert_eq!(ticks[0].payload["turn"], json!(TICK_EVERY));
}

#[test]
fn a_room_change_makes_a_tick_due_after_a_short_gap() {
    let (mut game, client) = begun();
    client.push(Response::canned_tool_use(
        "r2",
        "create_room",
        json!({"via_exit": "north", "back_exit": "south", "name": "Bell Tower", "region": "Saltmarsh",
               "description": "Tall.", "exits": {}, "contents": []}),
    ));
    client.push(Response::canned_text("The tower."));
    game.turn(&client, "go north", &mut |_| {}).unwrap();
    const { assert!(ROOM_CHANGE_GAP > 1) };
    assert!(!game.tick_due().unwrap(), "moved, but only one turn in");
    look(&mut game, &client);
    assert!(game.tick_due().unwrap(), "moved away from the opening room");

    // After a tick in the tower, staying put isn't a room change.
    quiet_tick(&mut game, &client);
    for _ in 0..ROOM_CHANGE_GAP {
        look(&mut game, &client);
    }
    assert!(!game.tick_due().unwrap());
}

#[test]
fn a_tick_changes_the_world_off_screen_and_the_change_is_found_on_return() {
    let (mut game, client, chapel, tower, crow, _) = two_rooms(false);
    let altar = game.world().contents(Some(chapel)).unwrap()[0];
    let player = player_id(game.world()).unwrap().unwrap();

    // Off-limits: the player's room, what's in it, the player, new rooms.
    let refused = [
        json!({"op": "move", "entity": crow, "to": chapel}),
        json!({"op": "set_prop", "entity": altar, "key": "cracked", "value": true}),
        json!({"op": "set_prop", "entity": player, "key": "cursed", "value": true}),
        json!({"op": "create", "name": "Crypt", "kind": "room"}),
    ];
    for (i, op) in refused.iter().enumerate() {
        client.push(Response::canned_tool_use(
            format!("x{i}"),
            "apply_patch",
            json!({"ops": [op], "note": "nope"}),
        ));
    }
    client.push(Response::canned_tool_use(
        "k1",
        "apply_patch",
        json!({"note": "The crow tore at the bell rope and left a feather.", "ops": [
            {"op": "set_prop", "entity": crow, "key": "mood", "value": "agitated"},
            {"op": "create", "name": "black feather", "kind": "item", "location": tower}
        ]}),
    ));
    client.push(Response::canned_text("The crow grows restless."));
    let before = client.requests().len();
    let seq_before = game.world().last_seq().unwrap();
    // Nothing is streamed: tick takes no on_text at all.
    let tick = game.tick(&client).unwrap();

    // Each refusal reached the director with a reason.
    let requests = client.requests()[before..].to_vec();
    for (i, req) in requests[1..=refused.len()].iter().enumerate() {
        match req.messages.last().unwrap().content.first().unwrap() {
            ContentBlock::ToolResult {
                content, is_error, ..
            } => {
                assert!(*is_error, "op {i} should be refused");
                assert!(
                    content.contains("off-screen") || content.contains("create rooms"),
                    "{content}"
                );
            }
            other => panic!("expected a tool result, got {other:?}"),
        }
    }
    // The tick's own request: tick tools only, a slice naming the off-limits room.
    let first = &requests[0];
    let names: Vec<&str> = first.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        names,
        ["read_bible", "query_entity", "search_events", "apply_patch"]
    );
    let slice = first_user_text(first);
    assert!(
        slice.contains(&format!(
            "The player is in #{chapel} Drowned Chapel (off-limits"
        )),
        "{slice}"
    );
    assert!(
        slice.contains(&format!("#{tower} Bell Tower (Saltmarsh)")),
        "{slice}"
    );
    assert!(
        slice.contains(&format!("#{crow} crow in #{tower}")),
        "{slice}"
    );
    assert!(slice.contains("> go south"), "{slice}");

    // Only the allowed patch applied, and it's in the log.
    assert_eq!(tick.summary, "The crow grows restless.");
    assert_eq!(
        tick.changes,
        vec![TickChange {
            note: "The crow tore at the bell rope and left a feather.".into(),
            rooms: vec![tower],
        }]
    );
    let crow_now = game.world().entity(crow).unwrap().unwrap();
    assert_eq!(crow_now.props["mood"], json!("agitated"));
    assert_eq!(crow_now.location, Some(tower));
    assert!(
        game.world()
            .entity(altar)
            .unwrap()
            .unwrap()
            .props
            .get("cracked")
            .is_none()
    );
    let events = game.world().events().unwrap();
    let tick_event = events.iter().rfind(|e| e.kind == EventKind::Tick).unwrap();
    assert_eq!(tick_event.payload["room"], json!(chapel));
    assert_eq!(
        tick_event.payload["changes"],
        json!([{"note": "The crow tore at the bell rope and left a feather.", "rooms": [tower]}])
    );
    // The change is an ordinary patch event, logged during the tick.
    assert!(events.iter().any(|e| e.kind == EventKind::Patch
        && e.seq > seq_before
        && e.payload["patch"]["ops"][0]["entity"] == json!(crow)
        && e.payload["patch"]["ops"][0]["value"] == json!("agitated")));
    assert!(
        !events
            .iter()
            .any(|e| e.kind == EventKind::Narration && e.seq > seq_before),
        "a tick narrates nothing to the player"
    );
    // The chapel's slice doesn't mention it; the tower's does.
    assert!(!local_slice(game.world()).unwrap().contains("off-screen"));
    client.push(Response::canned_tool_use(
        "m3",
        "move_player",
        json!({"exit": "north"}),
    ));
    client.push(Response::canned_text("Feathers everywhere."));
    game.turn(&client, "go north", &mut |_| {}).unwrap();
    let back = local_slice(game.world()).unwrap();
    assert!(back.contains("mood\":\"agitated"), "{back}");
    assert!(back.contains("black feather [item]"), "{back}");
    assert!(
        back.contains("Happened here off-screen (world ticks, newest first):\n- The crow tore at the bell rope and left a feather."),
        "{back}"
    );
    // The director finds it through search_events too.
    let found = game
        .world()
        .search_events("bell rope", &[EventKind::Tick], 5)
        .unwrap();
    assert_eq!(found.len(), 1);
}

#[test]
fn an_npc_leaving_a_room_is_noted_in_both_rooms() {
    let (mut game, client, _, tower, crow, loft) = two_rooms(true);
    let loft = loft.unwrap();
    client.push(Response::canned_tool_use(
        "k1",
        "apply_patch",
        json!({"note": "The crow flew up to the loft.", "ops": [{"op": "move", "entity": crow, "to": loft}]}),
    ));
    client.push(Response::canned_text("The crow moved."));
    let tick = game.tick(&client).unwrap();
    assert_eq!(tick.changes[0].rooms, vec![tower, loft]);
    assert_eq!(
        game.world().entity(crow).unwrap().unwrap().location,
        Some(loft)
    );
}

#[test]
fn inventory_tools_are_not_offered_in_a_tick() {
    let (mut game, client) = begun();
    client.push(Response::canned_tool_use("k1", "take", json!({"item": 1})));
    client.push(Response::canned_text("Nothing."));
    game.tick(&client).unwrap();
    let (msg, is_error) = last_tool_result(&client);
    assert!(is_error && msg.contains("no tool named"), "{msg}");
}

#[test]
fn a_session_with_ticks_replays_to_the_same_state() {
    let (mut game, client, _, tower, crow, _) = two_rooms(false);
    client.push(Response::canned_tool_use(
        "k1",
        "apply_patch",
        json!({"note": "A feather fell.", "ops": [
            {"op": "set_prop", "entity": crow, "key": "mood", "value": "agitated"},
            {"op": "create", "name": "black feather", "kind": "item", "location": tower}
        ]}),
    ));
    client.push(Response::canned_text("Restless."));
    game.tick(&client).unwrap();
    let replayed = game.world().replay_in_memory().unwrap();
    assert_eq!(replayed.state().unwrap(), game.world().state().unwrap());
    assert_eq!(replayed.events().unwrap(), game.world().events().unwrap());
}

// ---- stats and checks ---------------------------------------------------------

fn bible_only() -> Response {
    let mut r = bible_call();
    r.content
        .retain(|b| !matches!(b, ContentBlock::ToolUse(t) if t.name == "write_stats"));
    r
}

#[test]
fn the_bible_needs_a_stat_sheet() {
    let client = ScriptedClient::new([bible_only(), Response::canned_text("")]);
    let mut game = Game::new(World::open_in_memory().unwrap());
    assert!(matches!(
        game.create_bible(&client, "seed"),
        Err(GameError::NoStats)
    ));

    // A bad sheet is refused with the reason, and the director retries.
    let mut bad = crate::stats::sample_sheet();
    bad[0]["advancement"] = json!([]);
    let mut first = bible_only();
    first
        .content
        .push(ContentBlock::ToolUse(crate::llm::ToolUse {
            id: "s0".into(),
            name: "write_stats".into(),
            input: json!({"stats": bad}),
        }));
    let client = ScriptedClient::new([
        first,
        Response::canned_tool_use(
            "s1",
            "write_stats",
            json!({"stats": crate::stats::sample_sheet()}),
        ),
        Response::canned_text(""),
    ]);
    let mut game = Game::new(World::open_in_memory().unwrap());
    game.create_bible(&client, "seed").unwrap();
    let retry = &client.requests()[1];
    let refused = retry.messages[2].content.iter().any(|b| {
        matches!(b, ContentBlock::ToolResult { is_error: true, content, .. } if content.contains("advancement"))
    });
    assert!(refused, "{:?}", retry.messages[2]);
    assert_eq!(game.stats().unwrap().len(), 2);
}

#[test]
fn a_threshold_setback_is_narrated_and_play_goes_on() {
    let (mut game, client) = begun();
    let slice = local_slice(game.world()).unwrap();
    assert!(
        slice.contains("Stats (check, adjust_stat):\n- brawn 2 [0..5]"),
        "{slice}"
    );
    assert!(!slice.contains("_stats"), "{slice}");

    client.push(Response::canned_tool_use(
        "c",
        "check",
        json!({"stat": "nerve", "difficulty": 30, "reason": "stare into the bell shaft"}),
    ));
    client.push(Response::canned_tool_use(
        "a",
        "adjust_stat",
        json!({"stat": "nerve", "by": -3, "reason": "the bell shaft stared back"}),
    ));
    client.push(Response::canned_text("You run."));
    let out = game
        .turn(&client, "stare into the bell shaft", &mut |_| {})
        .unwrap();
    assert_eq!(out.narration, "You run.");
    let reqs = client.requests();
    let result = |i: usize| match reqs[i].messages.last().unwrap().content.first().unwrap() {
        ContentBlock::ToolResult {
            content, is_error, ..
        } => {
            assert!(!is_error, "{content}");
            serde_json::from_str::<Value>(content).unwrap()
        }
        other => panic!("{other:?}"),
    };
    // Even a 20 can't reach 30.
    assert_eq!(result(reqs.len() - 2)["result"], json!("failure"));
    let adjusted = result(reqs.len() - 1);
    assert_eq!(
        adjusted["changed"],
        json!([{"stat": "nerve", "from": 3, "to": 0}])
    );
    assert!(
        adjusted["thresholds"][0]["outcome"]
            .as_str()
            .unwrap()
            .contains("nerve breaks")
    );
    assert!(
        adjusted["note"]
            .as_str()
            .unwrap()
            .contains("the game goes on")
    );
    let player = player_id(game.world()).unwrap().unwrap();
    assert_eq!(
        game.world().entity(player).unwrap().unwrap().props["shaken"],
        json!(true)
    );
    let roll = game
        .world()
        .events()
        .unwrap()
        .into_iter()
        .find(|e| e.kind == EventKind::Roll)
        .unwrap();
    assert_eq!(roll.payload["reason"], json!("stare into the bell shaft"));

    // No game over: the next turn plays as usual.
    client.push(Response::canned_text("You catch your breath."));
    let next = game.turn(&client, "breathe", &mut |_| {}).unwrap();
    assert_eq!(next.narration, "You catch your breath.");
    assert!(
        local_slice(game.world())
            .unwrap()
            .contains("- nerve 0 [0..5]")
    );
    let replayed = game.world().replay_in_memory().unwrap();
    assert_eq!(replayed.state().unwrap(), game.world().state().unwrap());
}

#[test]
fn a_checked_rule_rolls_on_the_fast_path() {
    let (mut game, client, oak, elm) = with_doors();
    let mut mint = kick_mint(oak);
    if let ContentBlock::ToolUse(t) = &mut mint.content[0] {
        // nerve 3 + d20 always beats 1.
        t.input["check"] = json!({"stat": "nerve", "difficulty": 1,
                                  "failure": {"outcome": "You bounce off.", "adjust": [{"stat": "brawn", "by": -1}]}});
    }
    client.push(mint);
    client.push(Response::canned_text("The oak door bursts."));
    game.turn(&client, "kick the oak door", &mut |_| {})
        .unwrap();
    let (msg, is_error) = last_tool_result(&client);
    assert!(!is_error, "{msg}");
    let minted: Value = serde_json::from_str(&msg).unwrap();
    assert_eq!(minted["applied"]["check"]["result"], json!("success"));
    let rule = minted["rule"].as_i64().unwrap();

    let start = client.requests().len();
    client.push(interpret(
        json!([{"rule": rule, "bindings": {"target": elm}}]),
        true,
    ));
    client.push(Response::canned_text("The elm door gives way too."));
    let out = game
        .turn(&client, "kick the elm door", &mut |_| {})
        .unwrap();
    assert_eq!(out.fast_path, Some(rule));
    let narr = first_user_text(&client.requests()[start + 1]);
    assert!(
        narr.contains("Check: nerve d20 ") && narr.contains("against 1: \"success\""),
        "{narr}"
    );
    assert!(narr.contains("Outcome: The door bursts inward"), "{narr}");
    assert_eq!(game.world().count_events(EventKind::Roll).unwrap(), 2);
    let replayed = game.world().replay_in_memory().unwrap();
    assert_eq!(replayed.state().unwrap(), game.world().state().unwrap());
}
