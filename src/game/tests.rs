use serde_json::{Value, json};

use super::*;
use crate::llm::{ContentBlock, Response, ScriptedClient};

const SECRET: &str = "The abbot drowned the bell to hide the ledger";

fn bible_call() -> Response {
    Response::canned_tool_use(
        "b1",
        "write_bible",
        json!({
            "tone": "Damp, melancholy folk horror.",
            "regions": [{"name": "Saltmarsh", "description": "Reed beds around a sunken abbey."}],
            "factions": [{"name": "Eel-wives", "description": "Keep the old tithes."}],
            "npcs": [{"name": "Brother Ash", "description": "Last monk, lies kindly."}],
            "secrets": [{"name": "The bell", "description": SECRET}]
        }),
    )
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
    // The bible call only offers write_bible, and the seed is the prompt.
    let req = &client.requests()[0];
    assert_eq!(req.model, Model::Sonnet);
    assert_eq!(first_user_text(req), "A drowned abbey in the fens.");
    assert_eq!(
        req.tools
            .iter()
            .map(|t| t.name.as_str())
            .collect::<Vec<_>>(),
        ["write_bible"]
    );

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
    assert!(slice.contains("Carrying:\n- #5 tallow candle"), "{slice}");
    let replayed = game.world().replay_in_memory().unwrap();
    assert_eq!(replayed.state().unwrap(), game.world().state().unwrap());
}
