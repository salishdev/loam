use serde_json::{Value, json};

use super::exits;
use super::*;
use crate::llm::{ContentBlock, Response, ScriptedClient, ToolOutput, ToolUse};

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
                {"handle": "altar", "name": "altar", "kind": "feature", "props": {"material": "slate", "container": true}},
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

/// Entities with the world clock left out, for "nothing changed" checks on
/// turns that still spent time.
fn without_clock(mut entities: Vec<crate::world::Entity>) -> Vec<crate::world::Entity> {
    for e in &mut entities {
        e.props.remove("_clock");
    }
    entities
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
    let mut keys: Vec<_> = bible
        .props
        .keys()
        .filter(|k| !k.starts_with('_'))
        .cloned()
        .collect();
    keys.sort();
    let mut want = BIBLE_SECTIONS.to_vec();
    want.sort();
    assert_eq!(keys, want);
    // The clock starts at the default hour.
    assert_eq!(
        crate::time::now(game.world()).unwrap().to_string(),
        "day 1, 08:00 (morning)"
    );
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
        ["write_bible", "write_stats", "start_pressure"]
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
        slice.contains("altar [feature] {\"container\":true,\"material\":\"slate\"}"),
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
            "contents": [{"name": "crow", "kind": "crow", "props": {"mood": "wary", "acts": true}}]
        }),
    ));
    client.push(Response::canned_text("You climb to the tower."));
    game.turn(&client, "go north", &mut |_| {}).unwrap();
    let tower = player_room(&game);
    assert_ne!(tower, chapel);
    let tower_slice = local_slice(game.world()).unwrap();
    assert!(tower_slice.contains(&format!("south -> #{chapel} Drowned Chapel")));
    assert!(tower_slice.contains("crow [crow]"));

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
        exits::explored_rooms(game.world()).unwrap(),
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
    Response::canned_tool_use("k1", "mint_rule", kick_mint_input(target))
}

fn kick_mint_input(target: i64) -> Value {
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
    })
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

    // The log explains it: a judgment, a patch tagged with the rule, the
    // narration, then the clock moving on.
    let events = game.world().events().unwrap();
    let tail: Vec<_> = events.iter().rev().take(6).rev().collect();
    assert_eq!(
        tail.iter().map(|e| e.kind).collect::<Vec<_>>(),
        [
            EventKind::Input,
            EventKind::Judgment,
            EventKind::Patch,
            EventKind::Narration,
            EventKind::Patch,
            EventKind::Time
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
               "contents": [{"name": "crow", "kind": "crow", "props": {"mood": "wary", "acts": true}}]}),
    ));
    client.push(Response::canned_text("The tower."));
    game.turn(&client, "go north", &mut |_| {}).unwrap();
    let tower = player_room(&game);
    let crow = game.world().entities_marked(crate::world::ACTS).unwrap()[0];
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

/// Looks, at the default 10 minutes each, that make up `minutes`.
fn looks_for(minutes: u64) -> u64 {
    minutes / crate::time::DEFAULT_MINUTES
}

#[test]
fn a_tick_is_due_after_an_hour_of_world_time_in_the_same_room() {
    let (mut game, client) = begun();
    assert!(!game.tick_due().unwrap(), "no time has passed yet");
    for _ in 1..looks_for(TICK_MINUTES) {
        look(&mut game, &client);
        assert!(!game.tick_due().unwrap());
    }
    look(&mut game, &client);
    assert!(game.tick_due().unwrap(), "an hour has passed");

    // maybe_tick runs it and the hour starts again.
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
    assert_eq!(ticks[0].payload["turn"], json!(looks_for(TICK_MINUTES)));
    assert_eq!(
        ticks[0].payload["minute"],
        json!(crate::time::DEFAULT_START + TICK_MINUTES)
    );
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
    const { assert!(ROOM_CHANGE_MINUTES > crate::time::DEFAULT_MINUTES) };
    assert!(!game.tick_due().unwrap(), "moved, but only ten minutes in");
    look(&mut game, &client);
    assert!(game.tick_due().unwrap(), "moved away from the opening room");

    // After a tick in the tower, staying put isn't a room change.
    quiet_tick(&mut game, &client);
    for _ in 0..looks_for(ROOM_CHANGE_MINUTES) {
        look(&mut game, &client);
    }
    assert!(!game.tick_due().unwrap());
}

#[test]
fn a_long_sleep_makes_one_tick_that_reports_the_gap() {
    let (mut game, client) = begun();
    look(&mut game, &client);
    quiet_tick(&mut game, &client);
    // Sleep through the night: nine hours in one advance.
    crate::time::advance(game.world_mut(), 9 * 60, crate::time::Source::Director).unwrap();
    assert!(game.tick_due().unwrap());

    client.push(Response::canned_text("The night passed."));
    let before = client.requests().len();
    game.maybe_tick(&client).unwrap().expect("tick was due");
    let requests = client.requests()[before..].to_vec();
    assert_eq!(requests.len(), 1, "one tick, one call");
    let slice = match requests[0].messages[0].content.first().unwrap() {
        ContentBlock::Text { text } => text.clone(),
        other => panic!("expected the slice, got {other:?}"),
    };
    assert!(
        slice.contains("Time: day 1, 17:10 (dusk). 9 h have passed since the last tick."),
        "{slice}"
    );
    let system = requests[0].system.as_deref().unwrap_or_default();
    assert!(system.contains("long gaps"), "{system}");

    // One tick covers the whole night: nothing more is due.
    assert!(!game.tick_due().unwrap());
    assert_eq!(game.maybe_tick(&client).unwrap(), None);
}

#[test]
fn the_first_tick_slice_counts_from_the_start_of_the_story() {
    let (mut game, client) = begun();
    look(&mut game, &client);
    look(&mut game, &client);
    quiet_tick(&mut game, &client);
    let slice = match client.requests().last().unwrap().messages[0]
        .content
        .first()
        .unwrap()
    {
        ContentBlock::Text { text } => text.clone(),
        other => panic!("expected the slice, got {other:?}"),
    };
    assert!(
        slice.contains("20 min have passed since the story began."),
        "{slice}"
    );
}

#[test]
fn a_tick_logged_without_a_minute_counts_as_minute_zero() {
    let (mut game, client) = begun();
    look(&mut game, &client);
    // A tick from before ticks recorded the clock, in the opening room.
    let room = player_room(&game);
    game.world()
        .log_event(
            EventKind::Tick,
            &json!({"turn": 1, "room": room, "summary": "", "changes": []}),
        )
        .unwrap();
    assert!(game.tick_due().unwrap(), "08:10 is long after minute 0");
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
        [
            "read_bible",
            "query_entity",
            "search_events",
            "apply_patch",
            "start_pressure",
            "adjust_pressure",
            "resolve_pressure"
        ]
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

#[test]
fn director_relations_show_in_query_entity_slice_and_glossary() {
    let (mut game, client) = begun();
    let chapel = player_room(&game);
    let w = game.world();
    let altar = w
        .contents(Some(chapel))
        .unwrap()
        .into_iter()
        .find(|id| w.entity(*id).unwrap().unwrap().name == "altar")
        .unwrap();
    client.push(Response::canned_tool_use(
        "p",
        "apply_patch",
        json!({"ops": [
            {"op": "create", "handle": "hound", "name": "Hound", "kind": "hound", "location": chapel,
             "props": {"acts": true}},
            {"op": "relate", "subject": "hound", "rel": "guards", "object": altar,
             "props": {"since": "dawn"}},
        ]}),
    ));
    client.push(Response::canned_tool_use(
        "q",
        "query_entity",
        json!({"id": altar}),
    ));
    client.push(Response::canned_text("x"));
    game.turn(&client, "whistle", &mut |_| {}).unwrap();
    let (msg, is_error) = last_tool_result(&client);
    assert!(!is_error, "{msg}");
    let v: Value = serde_json::from_str(&msg).unwrap();
    assert_eq!(v["relations"]["from"], json!([]));
    assert_eq!(v["relations"]["to"][0]["rel"], json!("guards"));
    assert_eq!(v["relations"]["to"][0]["subject"]["name"], json!("Hound"));
    assert_eq!(v["relations"]["to"][0]["props"], json!({"since": "dawn"}));

    // The director can't use engine-owned relation names.
    client.push(Response::canned_tool_use(
        "p2",
        "apply_patch",
        json!({"ops": [{"op": "relate", "subject": altar, "rel": "_pins", "object": chapel}]}),
    ));
    client.push(Response::canned_text("x"));
    game.turn(&client, "cheat", &mut |_| {}).unwrap();
    let (msg, is_error) = last_tool_result(&client);
    assert!(is_error && msg.contains("engine-owned"), "{msg}");

    assert!(
        crate::glossary::terms(game.world())
            .unwrap()
            .contains_key("guards")
    );
    let slice = local_slice(game.world()).unwrap();
    let hound = game.world().relations_to(altar, None).unwrap()[0].subject;
    assert!(
        slice.contains(&format!(
            "Relations here:\n- #{hound} Hound -guards-> #{altar} altar {{\"since\":\"dawn\"}}"
        )),
        "{slice}"
    );

    // Engine-owned relations are left to their modules, not the slice.
    game.world_mut()
        .apply_engine_patch(&crate::world::Patch::new(vec![crate::world::Op::Relate {
            subject: hound.into(),
            rel: "_pins".into(),
            object: altar.into(),
            props: Default::default(),
        }]))
        .unwrap();
    assert!(!local_slice(game.world()).unwrap().contains("_pins"));
}

// ---- exits and stub rooms ---------------------------------------------------

/// The exits of `room` as `(dir, label, to, unexplored)`.
fn exit_rows(game: &Game, room: i64) -> Vec<(String, String, i64, bool)> {
    exits::exits(game.world(), room)
        .unwrap()
        .into_iter()
        .map(|e| (e.dir, e.label, e.to, e.unexplored))
        .collect()
}

#[test]
fn unexplored_exits_lead_to_stub_rooms_that_create_room_fills_in() {
    let (mut game, client) = begun();
    let chapel = player_room(&game);
    let rows = exit_rows(&game, chapel);
    assert_eq!(rows.len(), 1);
    let (dir, label, stub, unexplored) = rows[0].clone();
    assert_eq!(
        (dir.as_str(), label.as_str(), unexplored),
        ("north", "a reed path toward the bell tower", true)
    );
    let s = game.world().entity(stub).unwrap().unwrap();
    assert_eq!(s.kind, ROOM_KIND);
    assert_eq!(s.name, "a reed path toward the bell tower");
    assert_eq!(s.location, None);
    assert!(exits::is_stub(game.world(), stub).unwrap());
    // No exits prop anywhere: exits are relations.
    assert!(!s.props.contains_key("exits"));
    assert!(
        !game
            .world()
            .entity(chapel)
            .unwrap()
            .unwrap()
            .props
            .contains_key("exits")
    );

    client.push(Response::canned_tool_use(
        "r2",
        "create_room",
        json!({"via_exit": "north", "back_exit": "south", "name": "Bell Tower", "region": "Saltmarsh",
               "description": "A leaning tower.", "exits": {"up": "rotten stairs", "east": "a causeway"},
               "contents": [{"name": "crow", "kind": "crow", "props": {"mood": "wary", "acts": true}}]}),
    ));
    client.push(Response::canned_text("The tower."));
    game.turn(&client, "go north", &mut |_| {}).unwrap();
    let (msg, is_error) = last_tool_result(&client);
    assert!(!is_error, "{msg}");

    // The stub became the room: same id, filled in, marker gone.
    let tower = player_room(&game);
    assert_eq!(tower, stub);
    let t = game.world().entity(tower).unwrap().unwrap();
    assert_eq!(t.name, "Bell Tower");
    assert_eq!(t.props["description"], json!("A leaning tower."));
    assert_eq!(t.props["region"], json!("Saltmarsh"));
    assert!(!t.props.contains_key("_unexplored"));
    let crow = game.world().entities_marked(crate::world::ACTS).unwrap()[0];
    assert_eq!(
        game.world().entity(crow).unwrap().unwrap().location,
        Some(tower)
    );
    let v: Value = serde_json::from_str(&msg).unwrap();
    assert_eq!(v["room"], json!(tower));
    assert_eq!(v["contents"][0]["name"], json!("crow"));

    // Both ways are explored now; the further exits lead to new stubs.
    assert_eq!(
        exit_rows(&game, chapel),
        vec![("north".into(), "Bell Tower".into(), tower, false)]
    );
    let rows = exit_rows(&game, tower);
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows[1],
        ("south".into(), "Drowned Chapel".into(), chapel, false)
    );
    for (i, (dir, hint)) in [(0, ("east", "a causeway")), (2, ("up", "rotten stairs"))] {
        let (d, l, to, unexplored) = &rows[i];
        assert_eq!((d.as_str(), l.as_str(), *unexplored), (dir, hint, true));
        assert_eq!(game.world().entity(*to).unwrap().unwrap().name, hint);
    }
    assert_eq!(
        exits::explored_rooms(game.world()).unwrap(),
        vec![chapel, tower]
    );

    // The slice shows exits on their own line, not as relations.
    let slice = local_slice(game.world()).unwrap();
    assert!(
        slice.contains(&format!(
            "Exits: east -> unexplored (a causeway); south -> #{chapel} Drowned Chapel; \
             up -> unexplored (rotten stairs)"
        )),
        "{slice}"
    );
    assert!(!slice.contains("-exit->"), "{slice}");

    let replayed = game.world().replay_in_memory().unwrap();
    assert_eq!(replayed.state().unwrap(), game.world().state().unwrap());
}

#[test]
fn create_room_refuses_bad_exits_and_engine_props() {
    let (mut game, client) = begun();
    let chapel = player_room(&game);
    let before = game.world().state().unwrap();
    let tries = [
        (
            json!({"via_exit": "west", "back_exit": "east", "name": "X", "region": "Saltmarsh",
                "description": "", "exits": {}, "contents": []}),
            "no exit \"west\"",
        ),
        (
            json!({"via_exit": "north", "name": "X", "region": "Saltmarsh",
                "description": "", "exits": {}, "contents": []}),
            "back_exit is required",
        ),
        (
            json!({"via_exit": "north", "back_exit": "south", "name": "X", "region": "Saltmarsh",
                "description": "", "exits": {"south": "again"}, "contents": []}),
            "also listed",
        ),
        (
            json!({"via_exit": "north", "back_exit": "south", "name": "X", "region": "Saltmarsh",
                "description": "", "exits": {}, "contents": [{"name": "y", "kind": "item", "props": {"_carrier": true}}]}),
            "engine-owned",
        ),
    ];
    for (i, (input, why)) in tries.into_iter().enumerate() {
        client.push(Response::canned_tool_use(
            format!("r{i}"),
            "create_room",
            input,
        ));
        client.push(Response::canned_text("x"));
        game.turn(&client, "go north", &mut |_| {}).unwrap();
        let (msg, is_error) = last_tool_result(&client);
        assert!(is_error && msg.contains(why), "{why}: {msg}");
        assert_eq!(player_room(&game), chapel);
    }
    // Nothing changed but the log.
    assert_eq!(
        without_clock(game.world().state().unwrap().entities),
        without_clock(before.entities)
    );
    assert_eq!(game.world().state().unwrap().relations, before.relations);
}

#[test]
fn a_rule_effect_that_relates_an_exit_makes_it_walkable() {
    let (mut game, client) = begun();
    let chapel = player_room(&game);
    let w = game.world_mut();
    let a = w
        .apply_engine_patch(&crate::world::Patch::new(
            serde_json::from_value(json!([
                {"op": "create", "handle": "crypt", "name": "Crypt", "kind": "room",
                 "props": {"description": "Bones.", "region": "Saltmarsh"}},
                {"op": "create", "handle": "lever", "name": "lever", "kind": "feature", "location": chapel}
            ]))
            .unwrap(),
        ))
        .unwrap();
    let (crypt, lever) = (a.handles["crypt"], a.handles["lever"]);

    // No way down yet.
    client.push(Response::canned_tool_use(
        "m0",
        "move_player",
        json!({"exit": "down"}),
    ));
    client.push(Response::canned_text("x"));
    game.turn(&client, "go down", &mut |_| {}).unwrap();
    let (msg, is_error) = last_tool_result(&client);
    assert!(is_error && msg.contains("no exit \"down\""), "{msg}");

    client.push(Response::canned_tool_use(
        "k1",
        "mint_rule",
        json!({
            "name": "Pulling the chapel lever",
            "trigger": "pull <target>",
            "roles": {"target": "the lever"},
            "conditions": [{"check": "kind", "role": "target", "is": "feature"}],
            "effects": [
                {"op": "relate", "subject": "room", "rel": "exit", "object": crypt,
                 "props": {"dir": "down", "label": "Crypt"}},
                {"op": "relate", "subject": crypt, "rel": "exit", "object": "room",
                 "props": {"dir": "up", "label": "Drowned Chapel"}}
            ],
            "outcome": "A slab grinds aside, opening stairs down.",
            "apply_to": {"target": lever}
        }),
    ));
    client.push(Response::canned_text("A slab grinds aside."));
    game.turn(&client, "pull the lever", &mut |_| {}).unwrap();
    let (msg, is_error) = last_tool_result(&client);
    assert!(!is_error, "{msg}");
    assert_eq!(
        exit_rows(&game, chapel)[0],
        ("down".into(), "Crypt".into(), crypt, false)
    );

    client.push(interpret(json!([]), true));
    client.push(Response::canned_tool_use(
        "m1",
        "move_player",
        json!({"exit": "down"}),
    ));
    client.push(Response::canned_text("Down you go."));
    game.turn(&client, "go down", &mut |_| {}).unwrap();
    let (msg, is_error) = last_tool_result(&client);
    assert!(!is_error, "{msg}");
    assert_eq!(player_room(&game), crypt);
    client.push(interpret(json!([]), true));
    client.push(Response::canned_tool_use(
        "m2",
        "move_player",
        json!({"exit": "up"}),
    ));
    client.push(Response::canned_text("Up."));
    game.turn(&client, "go up", &mut |_| {}).unwrap();
    assert_eq!(player_room(&game), chapel);

    // An exit must join two rooms and can't reuse a dir.
    for (i, (props_obj, why)) in [
        (
            json!({"subject": chapel, "object": lever, "props": {"dir": "in", "label": "x"}}),
            "joins two rooms",
        ),
        (
            json!({"subject": chapel, "object": crypt, "props": {"label": "x"}}),
            "dir",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let mut op = props_obj;
        op["op"] = json!("relate");
        op["rel"] = json!("exit");
        client.push(interpret(json!([]), true));
        client.push(Response::canned_tool_use(
            format!("p{i}"),
            "apply_patch",
            json!({"ops": [op]}),
        ));
        client.push(Response::canned_text("x"));
        game.turn(&client, "dig", &mut |_| {}).unwrap();
        let (msg, is_error) = last_tool_result(&client);
        assert!(is_error && msg.contains(why), "{why}: {msg}");
    }
    let replayed = game.world().replay_in_memory().unwrap();
    assert_eq!(replayed.state().unwrap(), game.world().state().unwrap());
}

#[test]
fn destroying_a_room_removes_its_exits() {
    let (mut game, client, _, tower, _, loft) = two_rooms(true);
    let loft = loft.unwrap();
    assert!(exit_rows(&game, tower).iter().any(|e| e.2 == loft));
    client.push(Response::canned_tool_use(
        "d",
        "apply_patch",
        json!({"ops": [{"op": "destroy", "entity": loft}]}),
    ));
    client.push(Response::canned_text("The loft collapses."));
    game.turn(&client, "listen", &mut |_| {}).unwrap();
    let (msg, is_error) = last_tool_result(&client);
    assert!(!is_error, "{msg}");
    assert!(game.world().relations_to(loft, None).unwrap().is_empty());
    assert!(game.world().relations_from(loft, None).unwrap().is_empty());
    assert!(exit_rows(&game, tower).iter().all(|e| e.0 != "up"));
    let patch = game
        .world()
        .events()
        .unwrap()
        .into_iter()
        .rfind(|e| e.kind == EventKind::Patch && e.payload.get("cascaded").is_some())
        .unwrap();
    let cascaded = patch.payload["cascaded"].as_array().unwrap();
    assert_eq!(cascaded.len(), 2, "{cascaded:?}");
    assert!(cascaded.iter().all(|c| c["rel"] == json!("exit")));
}

#[test]
fn ticks_and_npc_listings_skip_stub_rooms() {
    let (mut game, client, _, tower, crow, _) = two_rooms(false);
    let stub = exit_rows(&game, tower)
        .into_iter()
        .find(|e| e.0 == "up")
        .unwrap()
        .2;

    // An NPC sent into a stub is nowhere yet.
    game.world_mut()
        .apply_patch(&crate::world::Patch::new(vec![crate::world::Op::Move {
            entity: crow.into(),
            to: Some(stub.into()),
        }]))
        .unwrap();
    let sighting = npcs(game.world()).unwrap().remove(0);
    assert_eq!(sighting.room, None);

    // The tick lists only explored rooms and can't touch stubs.
    client.push(Response::canned_tool_use(
        "x",
        "apply_patch",
        json!({"ops": [{"op": "set_prop", "entity": stub, "key": "dank", "value": true}], "note": "n"}),
    ));
    client.push(Response::canned_text("Nothing."));
    let before = client.requests().len();
    game.tick(&client).unwrap();
    let prompt = first_user_text(&client.requests()[before]);
    assert!(prompt.contains("Bell Tower"), "{prompt}");
    assert!(
        !prompt.contains("rotten stairs (") && !prompt.contains(&format!("#{stub} ")),
        "{prompt}"
    );
    let (msg, is_error) = last_tool_result(&client);
    assert!(is_error && msg.contains("unexplored"), "{msg}");
}

#[test]
fn a_related_rule_takes_the_fast_path_only_while_the_relation_holds() {
    let (mut game, client, oak, _) = with_doors();
    let room = player_room(&game);
    let patch = |ops: Value| crate::world::Patch::new(serde_json::from_value(ops).unwrap());
    let a = game
        .world
        .apply_patch(&patch(json!([
            {"op": "create", "handle": "hound", "name": "hound", "kind": "beast", "location": room}
        ])))
        .unwrap();
    let hound = a.handles["hound"];
    let guards = patch(json!([{"op": "relate", "subject": hound, "rel": "guards", "object": oak}]));
    game.world.apply_patch(&guards).unwrap();
    let def = serde_json::from_value(json!({
        "name": "Approaching a guarded door",
        "trigger": "approach or touch <door>",
        "roles": {"door": "the door", "guard": "what guards it"},
        "conditions": [{"check": "related", "role": "guard", "rel": "guards", "other": "door"}],
        "effects": [{"op": "set_prop", "entity": "guard", "key": "snarling", "value": true}],
        "outcome": "The guard bares its teeth."
    }))
    .unwrap();
    let (rule, _) = crate::rules::mint(&mut game.world, def, "")
        .unwrap()
        .unwrap();
    let matched = || {
        interpret(
            json!([{"rule": rule.id, "bindings": {"door": oak, "guard": hound}}]),
            true,
        )
    };

    // While the hound guards the door, the engine applies the rule.
    client.push(matched());
    client.push(Response::canned_text("The hound snarls."));
    let turn = game
        .turn(&client, "touch the oak door", &mut |_| {})
        .unwrap();
    assert_eq!(turn.fast_path, Some(rule.id));
    let props = game.world().entity(hound).unwrap().unwrap().props;
    assert_eq!(props["snarling"], json!(true));

    // Once it stops guarding, the director decides, told why.
    game.world
        .apply_patch(&patch(json!([
            {"op": "unrelate", "subject": hound, "rel": "guards", "object": oak}
        ])))
        .unwrap();
    client.push(matched());
    client.push(Response::canned_text("The hound ignores you."));
    let turn = game
        .turn(&client, "touch the oak door", &mut |_| {})
        .unwrap();
    assert_eq!(turn.fast_path, None);
    let director = first_user_text(client.requests().last().unwrap());
    assert!(
        director.contains("no guards relation runs from guard to door"),
        "{director}"
    );
}

#[test]
fn reserved_kinds_name_the_engine_kinds() {
    use crate::world::RESERVED_KINDS;
    for k in [
        ROOM_KIND,
        PLAYER_KIND,
        BIBLE_KIND,
        crate::rules::RULE_KIND,
        crate::quest::QUEST_KIND,
        crate::glossary::GLOSSARY_KIND,
        crate::pressure::PRESSURE_KIND,
    ] {
        assert!(RESERVED_KINDS.contains(&k), "{k}");
    }
}

#[test]
fn the_director_cannot_create_entities_of_reserved_kinds() {
    let (mut game, client) = begun();
    let room = player_room(&game);
    let before = game.world().state().unwrap();
    for (i, kind) in ["quest", "room", "player", "rule", "pressure"]
        .into_iter()
        .enumerate()
    {
        client.push(Response::canned_tool_use(
            format!("p{i}"),
            "apply_patch",
            json!({"ops": [{"op": "create", "name": "fake", "kind": kind, "location": room}]}),
        ));
        client.push(Response::canned_text("x"));
        game.turn(&client, "cheat", &mut |_| {}).unwrap();
        let (msg, is_error) = last_tool_result(&client);
        assert!(is_error && msg.contains("is reserved"), "{msg}");
    }
    // create_room's contents can't smuggle one in either.
    client.push(Response::canned_tool_use(
        "r",
        "create_room",
        json!({
            "via_exit": "north", "back_exit": "south", "name": "Bell Tower",
            "region": "Saltmarsh", "description": "A tower.", "exits": {},
            "contents": [{"name": "fake quest", "kind": "quest"}]
        }),
    ));
    client.push(Response::canned_text("x"));
    game.turn(&client, "go north", &mut |_| {}).unwrap();
    let (msg, is_error) = last_tool_result(&client);
    assert!(is_error && msg.contains("is reserved"), "{msg}");
    assert_eq!(
        without_clock(game.world().state().unwrap().entities),
        without_clock(before.entities)
    );
    assert!(crate::quest::quests(game.world()).unwrap().is_empty());
}

#[test]
fn acts_beings_hold_items_and_npcs_are_found_by_acts() {
    let (mut game, client) = begun();
    let room = player_room(&game);
    client.push(Response::canned_tool_use(
        "p",
        "apply_patch",
        json!({"ops": [
            {"op": "create", "handle": "sword", "name": "talking sword", "kind": "sword",
             "location": room, "props": {"acts": true}},
            {"op": "create", "handle": "heron", "name": "heron", "kind": "heron",
             "location": room},
            {"op": "create", "name": "gem", "kind": "gem", "location": "sword"}
        ]}),
    ));
    client.push(Response::canned_text("x"));
    game.turn(&client, "look", &mut |_| {}).unwrap();
    let (msg, is_error) = last_tool_result(&client);
    assert!(!is_error, "{msg}");
    let names: Vec<String> = npcs(game.world())
        .unwrap()
        .into_iter()
        .map(|s| s.npc.name)
        .collect();
    // The sword acts, whatever its kind; the heron doesn't.
    assert_eq!(names, ["talking sword"]);

    // Nothing goes into the heron until it is marked.
    let heron = game
        .world()
        .state()
        .unwrap()
        .entities
        .into_iter()
        .find(|e| e.name == "heron")
        .unwrap()
        .id;
    client.push(Response::canned_tool_use(
        "q",
        "apply_patch",
        json!({"ops": [{"op": "create", "name": "fish", "kind": "fish", "location": heron}]}),
    ));
    client.push(Response::canned_text("x"));
    game.turn(&client, "feed the heron", &mut |_| {}).unwrap();
    let (msg, is_error) = last_tool_result(&client);
    assert!(is_error && msg.contains("not a container"), "{msg}");
}

#[test]
fn every_turn_spends_world_time_and_the_slice_shows_it() {
    let (mut game, client) = begun();
    // The opening costs nothing.
    assert!(crate::time::recent(game.world(), 1).unwrap().is_empty());
    let first = first_user_text(client.requests().last().unwrap());
    assert!(first.contains("Time: day 1, 08:00 (morning)\n"), "{first}");

    client.push(Response::canned_text("You wait."));
    game.turn(&client, "wait", &mut |_| {}).unwrap();
    client.push(Response::canned_text("You wait more."));
    game.turn(&client, "wait", &mut |_| {}).unwrap();
    assert_eq!(
        crate::time::now(game.world()).unwrap().to_string(),
        "day 1, 08:20 (morning)"
    );
    let text = first_user_text(client.requests().last().unwrap());
    assert!(
        text.contains("Time: day 1, 08:10 (morning); your last action took 10 min\n"),
        "{text}"
    );
    let replayed = game.world().replay_in_memory().unwrap();
    assert_eq!(replayed.state().unwrap(), game.world().state().unwrap());
}

#[test]
fn the_bible_can_choose_the_opening_hour_and_hides_the_clock() {
    let mut world = World::open_in_memory().unwrap();
    let inventory = Inventory::new(DEFAULT_SLOTS, stats::carry_limit);
    let call = |world: &mut World, phase, name: &str, input: Value| {
        let call = ToolUse {
            id: "c".into(),
            name: name.into(),
            input,
        };
        tools::dispatch(world, &inventory, 7, phase, &call).unwrap()
    };
    let bible = |start: &str| {
        json!({
            "tone": "t", "start": start,
            "regions": [{"name": "r", "description": "d"}],
            "factions": [{"name": "f", "description": "d"}],
            "npcs": [{"name": "n", "description": "d"}],
            "secrets": [{"name": "s", "description": "d"}]
        })
    };
    match call(&mut world, Phase::Bible, "write_bible", bible("25:00")) {
        ToolOutput::Err(e) => assert!(e.contains("not a 24-hour"), "{e}"),
        ok => panic!("{ok:?}"),
    }
    assert!(matches!(
        call(&mut world, Phase::Bible, "write_bible", bible("21:30")),
        ToolOutput::Ok(_)
    ));
    assert_eq!(
        crate::time::now(&world).unwrap().to_string(),
        "day 1, 21:30 (evening)"
    );
    // The clock is no bible section: not listed, not readable.
    match call(
        &mut world,
        Phase::Play,
        "read_bible",
        json!({"section": "_clock"}),
    ) {
        ToolOutput::Err(e) => assert!(
            e.ends_with("sections: factions, npcs, regions, secrets, tone"),
            "{e}"
        ),
        ok => panic!("{ok:?}"),
    }
    let slice = local_slice(&world).unwrap();
    assert!(!slice.contains("_clock"), "{slice}");
}

fn interpret_taking(matches: Value, clean: bool, duration: &str) -> Response {
    Response::canned_tool_use(
        "i",
        "interpret",
        json!({"matches": matches, "clean": clean, "duration": duration}),
    )
}

fn pass_time(minutes: i64) -> Response {
    Response::canned_tool_use(
        "p",
        "pass_time",
        json!({"minutes": minutes, "reason": "test"}),
    )
}

/// The last advance: minutes and source.
fn last_advance(game: &Game) -> (u64, String) {
    let a = crate::time::recent(game.world(), 1).unwrap().remove(0);
    (a.minutes, a.source)
}

fn rule_minutes(game: &Game, id: i64) -> Option<u32> {
    crate::rules::get(game.world(), id)
        .unwrap()
        .unwrap()
        .def
        .minutes
}

/// A kick rule minted without applying it, so it carries no duration.
fn untimed_kick_rule(game: &mut Game, client: &ScriptedClient, oak: i64) -> i64 {
    let mut input = kick_mint_input(oak);
    input.as_object_mut().unwrap().remove("apply_to");
    client.push(Response::canned_tool_use("k1", "mint_rule", input));
    client.push(Response::canned_text("You study the door."));
    game.turn(client, "study the oak door", &mut |_| {})
        .unwrap();
    let (msg, is_error) = last_tool_result(client);
    assert!(!is_error, "{msg}");
    serde_json::from_str::<Value>(&msg).unwrap()["rule"]
        .as_i64()
        .unwrap()
}

#[test]
fn a_turn_with_nothing_to_go_on_spends_the_default() {
    let (mut game, client) = begun();
    client.push(Response::canned_text("You look about."));
    game.turn(&client, "look", &mut |_| {}).unwrap();
    assert_eq!(last_advance(&game), (10, "default".into()));
}

#[test]
fn the_interpreter_estimate_times_a_director_turn_and_is_in_its_note() {
    let (mut game, client, oak, _) = with_doors();
    untimed_kick_rule(&mut game, &client, oak);
    // The rule doesn't match a search: the director rules, told the estimate.
    client.push(interpret_taking(json!([]), false, "hour"));
    client.push(Response::canned_text("You search the pews."));
    game.turn(&client, "search the pews", &mut |_| {}).unwrap();
    let director = first_user_text(client.requests().last().unwrap());
    assert!(
        director.contains(
            "Engine note: The interpreter estimates this takes about 1 h (hour); call \
             pass_time if it took notably more or less.\nPlayer input: search the pews"
        ),
        "{director}"
    );
    assert_eq!(last_advance(&game), (60, "interpreter".into()));

    // A malformed estimate is ignored, and the matches still count.
    client.push(interpret_taking(json!([]), false, "fortnight"));
    client.push(Response::canned_text("You wait."));
    game.turn(&client, "wait", &mut |_| {}).unwrap();
    let director = first_user_text(client.requests().last().unwrap());
    assert!(!director.contains("Engine note"), "{director}");
    assert_eq!(last_advance(&game), (10, "default".into()));
}

#[test]
fn pass_time_beats_the_estimate_and_the_last_call_wins() {
    let (mut game, client, oak, _) = with_doors();
    untimed_kick_rule(&mut game, &client, oak);
    let before = crate::time::now(game.world()).unwrap();
    client.push(interpret_taking(json!([]), false, "minutes"));
    client.push(pass_time(30));
    client.push(pass_time(480));
    // Out of range: refused, and so ignored.
    client.push(pass_time(20_000));
    client.push(Response::canned_text("You sleep until dawn."));
    game.turn(&client, "sleep", &mut |_| {}).unwrap();
    assert_eq!(last_advance(&game), (480, "director".into()));
    assert_eq!(crate::time::now(game.world()).unwrap().0, before.0 + 480);
    let refused = game
        .world()
        .events()
        .unwrap()
        .into_iter()
        .rfind(|e| e.kind == EventKind::ToolCall)
        .unwrap();
    assert_eq!(refused.payload["is_error"], json!(true));
    assert!(
        refused.payload["result"]
            .as_str()
            .unwrap()
            .contains("minutes must be 0 to 10080"),
        "{}",
        refused.payload
    );
}

#[test]
fn a_rule_minted_with_apply_to_keeps_the_turns_final_duration() {
    let (mut game, client, oak, elm) = with_doors();
    // pass_time after the mint still counts: the duration is the turn's final one.
    client.push(kick_mint(oak));
    client.push(pass_time(25));
    client.push(Response::canned_text("The oak door bursts."));
    game.turn(&client, "kick the oak door", &mut |_| {})
        .unwrap();
    assert_eq!(last_advance(&game), (25, "director".into()));
    let rule = crate::rules::active_world(game.world()).unwrap()[0].clone();
    assert_eq!(rule.def.minutes, Some(25));
    assert_eq!(rule.version, 1, "timing a fresh rule is not an amendment");
    let timed = game
        .world()
        .events()
        .unwrap()
        .into_iter()
        .rfind(|e| e.kind == EventKind::RuleChange)
        .unwrap();
    assert_eq!(timed.payload["action"], json!("timed"));
    assert_eq!(timed.payload["rule"]["minutes"], json!(25));

    // The fast-path repeat costs exactly the rule's minutes, not the estimate.
    let before = crate::time::now(game.world()).unwrap();
    client.push(interpret_taking(
        json!([{"rule": rule.id, "bindings": {"target": elm}}]),
        true,
        "moment",
    ));
    client.push(Response::canned_text("The elm door gives way too."));
    let outcome = game
        .turn(&client, "kick the elm door", &mut |_| {})
        .unwrap();
    assert_eq!(outcome.fast_path, Some(rule.id));
    assert_eq!(last_advance(&game), (25, "rule".into()));
    assert_eq!(crate::time::now(game.world()).unwrap().0, before.0 + 25);

    let replayed = game.world().replay_in_memory().unwrap();
    assert_eq!(replayed.state().unwrap(), game.world().state().unwrap());
}

#[test]
fn a_rule_minted_with_its_own_minutes_keeps_them() {
    let (mut game, client, oak, _) = with_doors();
    let mut input = kick_mint_input(oak);
    input["minutes"] = json!(3);
    client.push(Response::canned_tool_use("k1", "mint_rule", input));
    client.push(pass_time(50));
    client.push(Response::canned_text("The oak door bursts."));
    game.turn(&client, "kick the oak door", &mut |_| {})
        .unwrap();
    let rule = crate::rules::active_world(game.world()).unwrap()[0].id;
    assert_eq!(rule_minutes(&game, rule), Some(3));
    assert_eq!(last_advance(&game), (50, "director".into()));

    // amend_rule changes them.
    client.push(interpret_taking(json!([]), false, "moment"));
    client.push(Response::canned_tool_use(
        "a",
        "amend_rule",
        json!({"id": rule, "reason": "slower", "minutes": 7}),
    ));
    client.push(Response::canned_text("Noted."));
    game.turn(&client, "think", &mut |_| {}).unwrap();
    assert_eq!(rule_minutes(&game, rule), Some(7));
}

#[test]
fn an_untimed_rule_on_the_fast_path_spends_the_estimate() {
    let (mut game, client, oak, elm) = with_doors();
    let rule = untimed_kick_rule(&mut game, &client, oak);
    assert_eq!(last_advance(&game), (10, "default".into()));
    assert_eq!(rule_minutes(&game, rule), None, "minted without apply_to");
    client.push(interpret_taking(
        json!([{"rule": rule, "bindings": {"target": elm}}]),
        true,
        "hour",
    ));
    client.push(Response::canned_text("You batter the elm door down."));
    let outcome = game
        .turn(&client, "kick the elm door", &mut |_| {})
        .unwrap();
    assert_eq!(outcome.fast_path, Some(rule));
    assert_eq!(last_advance(&game), (60, "interpreter".into()));
}

/// Whether the director's latest request carried the stall hint.
fn told_stalling(client: &ScriptedClient) -> bool {
    first_user_text(client.requests().last().unwrap()).contains(STALL_HINT)
}

#[test]
fn quiet_turns_in_one_room_add_the_stall_hint_and_a_patch_resets_it() {
    let (mut game, client) = begun();
    for n in 1..=STALL_TURNS {
        look(&mut game, &client);
        assert!(
            !told_stalling(&client),
            "turn {n} came after too few quiet turns"
        );
    }
    assert_eq!(stall_hint(game.world()).unwrap(), Some(STALL_HINT));
    // An off-screen tick between turns doesn't break the stall.
    quiet_tick(&mut game, &client);
    look(&mut game, &client);
    assert!(told_stalling(&client));

    // A turn that patches the world resets the count.
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
    client.push(Response::canned_text("The candle catches."));
    game.turn(&client, "light the candle", &mut |_| {}).unwrap();
    // The patching turn itself was still told: the hint never refuses.
    assert!(told_stalling(&client));
    assert_eq!(stall_hint(game.world()).unwrap(), None);
    for _ in 1..STALL_TURNS {
        look(&mut game, &client);
        assert!(!told_stalling(&client));
    }
    look(&mut game, &client);
    assert!(
        !told_stalling(&client),
        "only three quiet turns since the patch"
    );
    look(&mut game, &client);
    assert!(told_stalling(&client));
}

#[test]
fn a_room_change_resets_the_stall_hint() {
    let (mut game, client) = begun();
    for _ in 0..STALL_TURNS {
        look(&mut game, &client);
    }
    assert!(stall_hint(game.world()).unwrap().is_some());
    client.push(Response::canned_tool_use(
        "r2",
        "create_room",
        json!({"via_exit": "north", "back_exit": "south", "name": "Bell Tower", "region": "Saltmarsh",
               "description": "A leaning tower.", "exits": {}, "contents": []}),
    ));
    client.push(Response::canned_text("The tower."));
    game.turn(&client, "go north", &mut |_| {}).unwrap();
    assert_eq!(stall_hint(game.world()).unwrap(), None);
    look(&mut game, &client);
    assert!(!told_stalling(&client));
}

#[test]
fn the_stall_hint_survives_a_fork() {
    let (mut game, client) = begun();
    for _ in 0..STALL_TURNS {
        look(&mut game, &client);
    }
    let replayed = game.world().replay_in_memory().unwrap();
    assert_eq!(stall_hint(&replayed).unwrap(), Some(STALL_HINT));
}

#[test]
fn the_director_and_narrator_are_asked_to_let_time_show() {
    let (_, client) = begun();
    let system = client.requests().last().unwrap().system.clone().unwrap();
    assert!(system.contains("never state clock times"), "{system}");
    assert!(fast_path::NARRATOR_SYSTEM.contains("without stating clock times"));
}

// ---- pressures ---------------------------------------------------------------

#[test]
fn pressures_reach_the_director_but_never_the_narrator_and_replay() {
    let (mut game, client, oak, elm) = with_doors();
    // A novel turn mints the kick rule and starts a pressure on the elm door.
    client.push(kick_mint(oak));
    client.push(Response::canned_tool_use(
        "p1",
        "start_pressure",
        json!({"what": "The elm door's hinges rust through", "consequence": "it falls",
               "due_in_minutes": 30, "concerns": [elm]}),
    ));
    client.push(Response::canned_text(
        "The oak door bursts; the elm door creaks.",
    ));
    game.turn(&client, "kick the oak door", &mut |_| {})
        .unwrap();
    let (msg, is_error) = last_tool_result(&client);
    assert!(!is_error, "{msg}");
    let p = crate::pressure::active(game.world()).unwrap()[0].clone();
    assert_eq!(p.concerns, vec![elm]);

    // Next director turn: the pressure is listed, 20 min left, imminent.
    // (With a rule minted, Haiku interprets every turn first; it matches
    // nothing here.)
    let nothing = || interpret(json!([]), true);
    client.push(nothing());
    client.push(Response::canned_text("You wait."));
    game.turn(&client, "wait", &mut |_| {}).unwrap();
    let director = first_user_text(client.requests().last().unwrap());
    assert!(
        director.contains(&format!(
            "Pressures (nearest first; adjust_pressure, resolve_pressure):\n- pressure #{} \
             The elm door's hinges rust through [imminent] due day 1, 08:30 (morning) (in 20 min)",
            p.id
        )),
        "{director}"
    );
    assert!(!director.contains("overdue:"), "{director}");
    // The local slice (shared with the narrator and interpreter) has none of it.
    assert!(!local_slice(game.world()).unwrap().contains("pressure"));

    // A fast-path turn: neither Haiku nor the narrator hears of it.
    let start = client.requests().len();
    let rule = crate::rules::active_world(game.world()).unwrap()[0].id;
    client.push(interpret(
        json!([{"rule": rule, "bindings": {"target": elm}}]),
        true,
    ));
    client.push(Response::canned_text("The elm door gives way."));
    let out = game
        .turn(&client, "kick the elm door", &mut |_| {})
        .unwrap();
    assert_eq!(out.fast_path, Some(rule));
    for req in &client.requests()[start..] {
        let text = first_user_text(req);
        assert!(!text.contains("hinges rust"), "{text}");
    }

    // Overdue now (08:30, its due time): the director is nudged.
    client.push(nothing());
    client.push(Response::canned_text("Nothing."));
    game.turn(&client, "wait", &mut |_| {}).unwrap();
    let director = first_user_text(client.requests().last().unwrap());
    assert!(
        director.contains("[overdue]") && director.contains(crate::pressure::OVERDUE_NUDGE),
        "{director}"
    );
    // The director resolves it.
    client.push(nothing());
    client.push(Response::canned_tool_use(
        "p2",
        "resolve_pressure",
        json!({"id": p.id, "how": "the door was already broken down"}),
    ));
    client.push(Response::canned_text("Splinters."));
    game.turn(&client, "look", &mut |_| {}).unwrap();
    assert!(!last_tool_result(&client).1);
    assert!(crate::pressure::active(game.world()).unwrap().is_empty());
    client.push(nothing());
    client.push(Response::canned_text("Quiet."));
    game.turn(&client, "wait", &mut |_| {}).unwrap();
    let director = first_user_text(client.requests().last().unwrap());
    assert!(!director.contains("Pressures"), "{director}");

    let replayed = game.world().replay_in_memory().unwrap();
    assert_eq!(replayed.state().unwrap(), game.world().state().unwrap());
    assert_eq!(
        crate::pressure::pressures(&replayed).unwrap(),
        crate::pressure::pressures(game.world()).unwrap()
    );
}

#[test]
fn a_tick_sees_pressures_and_touches_only_off_screen_ones() {
    let (mut game, client, chapel, tower, crow, _) = two_rooms(false);
    let altar = game.world().contents(Some(chapel)).unwrap()[0];
    let world = game.world_mut();
    let near =
        crate::pressure::start(world, "The altar cracks", "relics spill", 5, &[altar]).unwrap();
    let far = crate::pressure::start(
        world,
        "The crow flies off",
        "the bell goes quiet",
        600,
        &[crow],
    )
    .unwrap();
    crate::time::advance(world, 10, crate::time::Source::Default).unwrap();

    let player = player_id(game.world()).unwrap().unwrap();
    let calls = [
        (
            "adjust_pressure",
            json!({"id": near, "minutes": 60, "reason": "x"}),
        ),
        ("resolve_pressure", json!({"id": near, "how": "x"})),
        (
            "start_pressure",
            json!({"what": "w", "consequence": "c", "due_in_minutes": 60, "concerns": [player]}),
        ),
        (
            "adjust_pressure",
            json!({"id": far, "minutes": -60, "reason": "the bell rope frays"}),
        ),
        (
            "start_pressure",
            json!({"what": "Rooks gather", "consequence": "the tower fills", "due_in_minutes": 120, "concerns": [tower]}),
        ),
    ];
    for (i, (name, input)) in calls.iter().enumerate() {
        client.push(Response::canned_tool_use(
            format!("t{i}"),
            *name,
            input.clone(),
        ));
    }
    client.push(Response::canned_text("The crow frets."));
    let before = client.requests().len();
    game.tick(&client).unwrap();
    let requests = client.requests()[before..].to_vec();

    // The tick slice lists both, nearest first, marking the on-screen one
    // and nudging the overdue one; the tick gets the pressure tools.
    let slice = first_user_text(&requests[0]);
    let lines: Vec<&str> = slice
        .lines()
        .filter(|l| l.starts_with("- pressure"))
        .collect();
    assert_eq!(lines.len(), 2, "{slice}");
    assert!(
        lines[0].starts_with(&format!("- pressure #{near} The altar cracks [overdue]"))
            && lines[0].contains("(on-screen: leave it to play)")
            && lines[0].ends_with(crate::pressure::OVERDUE_NUDGE),
        "{slice}"
    );
    assert!(
        lines[1].starts_with(&format!("- pressure #{far} The crow flies off [low]"))
            && !lines[1].contains("on-screen"),
        "{slice}"
    );
    let tools: Vec<&str> = requests[0].tools.iter().map(|t| t.name.as_str()).collect();
    for t in ["start_pressure", "adjust_pressure", "resolve_pressure"] {
        assert!(tools.contains(&t), "{tools:?}");
    }

    // On-screen calls are refused; off-screen ones apply.
    for (i, req) in requests[1..=calls.len()].iter().enumerate() {
        let (content, is_error) = match req.messages.last().unwrap().content.first().unwrap() {
            ContentBlock::ToolResult {
                content, is_error, ..
            } => (content.clone(), *is_error),
            other => panic!("expected a tool result, got {other:?}"),
        };
        if i < 3 {
            assert!(
                is_error && content.contains("leaves pressures on"),
                "{i}: {content}"
            );
        } else {
            assert!(!is_error, "{i}: {content}");
        }
    }
    let near = crate::pressure::pressure(game.world(), near)
        .unwrap()
        .unwrap();
    assert_eq!(near.status, crate::pressure::Status::Active);
    assert!(near.adjustments.is_empty());
    let far = crate::pressure::pressure(game.world(), far)
        .unwrap()
        .unwrap();
    assert_eq!(far.adjustments[0].minutes, -60);
    assert_eq!(crate::pressure::active(game.world()).unwrap().len(), 3);

    let replayed = game.world().replay_in_memory().unwrap();
    assert_eq!(replayed.state().unwrap(), game.world().state().unwrap());
}

#[test]
fn the_bible_can_open_under_pressure() {
    let mut bible = bible_call();
    bible.content.push(ContentBlock::ToolUse(ToolUse {
        id: "s".into(),
        name: "start_pressure".into(),
        input: json!({"what": "The tide turns", "consequence": "the causeway floods",
                      "due_in_minutes": 240}),
    }));
    let client = ScriptedClient::new([bible, Response::canned_text("")]);
    let mut game = Game::new(World::open_in_memory().unwrap());
    game.create_bible(&client, "A tidal island.").unwrap();
    let p = &crate::pressure::active(game.world()).unwrap()[0];
    assert_eq!(p.what, "The tide turns");
    assert_eq!(p.due.to_string(), "day 1, 12:00 (afternoon)");
}

#[test]
fn an_overdue_pressure_on_a_quest_puts_escalate_quest_in_the_director_slice() {
    let (mut game, client) = begun();
    let quest =
        crate::quest::spawn(game.world_mut(), "Drain the chapel", "water rises", &[]).unwrap();
    let p = crate::pressure::start(
        game.world_mut(),
        "The crypt floods",
        "the relics are lost",
        10,
        &[quest],
    )
    .unwrap();
    look(&mut game, &client);
    let director = first_user_text(client.requests().last().unwrap());
    assert!(!director.contains("escalate_quest"), "{director}");

    // Due now: the director is told to escalate the quest, which stays open.
    look(&mut game, &client);
    let director = first_user_text(client.requests().last().unwrap());
    let q = crate::quest::quest(game.world(), quest).unwrap().unwrap();
    assert!(
        director.contains(&format!("- pressure #{p} The crypt floods [overdue]"))
            && director.contains(&crate::pressure::quest_nudge(&q))
            && director.contains(&format!("escalate_quest #{quest}")),
        "{director}"
    );
    assert_eq!(q.status, crate::quest::Status::Open);
}

// ---- call records ------------------------------------------------------------

fn with_usage(mut response: Response, input: u64, output: u64, read: u64) -> Response {
    response.usage = crate::llm::Usage {
        input_tokens: input,
        output_tokens: output,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: read,
    };
    response
}

fn roles(calls: &[crate::world::StoredCall]) -> Vec<&str> {
    calls.iter().map(|c| c.call.role.as_str()).collect()
}

#[test]
fn every_model_call_is_recorded_with_its_role_usage_and_cost() {
    let (mut game, client, oak, elm) = with_doors();
    // The bible and the opening belong to no turn.
    let early = game.world().calls().unwrap();
    assert_eq!(
        roles(&early),
        ["bible", "bible", "director", "director", "director"]
    );
    assert!(early.iter().all(|c| c.turn_seq.is_none()));

    // A director turn: one row per call of the tool loop.
    client.push(kick_mint(oak));
    client.push(with_usage(
        Response::canned_text("The oak door bursts."),
        300,
        80,
        4000,
    ));
    game.turn(&client, "kick the oak door", &mut |_| {})
        .unwrap();
    let first = game
        .world()
        .last_event(EventKind::Input)
        .unwrap()
        .unwrap()
        .seq;
    let calls = game.world().turn_calls(first).unwrap();
    assert_eq!(roles(&calls), ["director", "director"]);
    let last = &calls[1].call;
    assert_eq!(last.model, "claude-sonnet-5-5");
    assert_eq!(last.effort.as_deref(), Some("medium"));
    assert_eq!(last.usage.cache_read_input_tokens, 4000);
    // 300 in at $2, 80 out at $10, 4000 cache reads at $0.10 per million.
    assert!((last.cost_usd.unwrap() - 0.0018).abs() < 1e-12);
    assert_eq!(last.response["content"][0]["text"], "The oak door bursts.");
    // The second call carries the first call's tool use and its result.
    assert_eq!(last.request["messages"].as_array().unwrap().len(), 3);
    assert!(last.request.get("system").is_none());

    // A fast-path turn with a quest to judge: interpreter, streamed
    // narrator (recorded once the stream ends), judge.
    let rule = crate::rules::active_world(game.world()).unwrap()[0].id;
    let q = crate::quest::spawn(&mut game.world, "Through", "It bars the way", &[elm]).unwrap();
    crate::quest::reveal_goal(&mut game.world, q, "Break the elm door", &[elm]).unwrap();
    client.push(with_usage(
        interpret(json!([{"rule": rule, "bindings": {"target": elm}}]), true),
        900,
        40,
        0,
    ));
    client.push(Response::canned_text("The elm door gives way."));
    client.push(Response::canned_tool_use(
        "j",
        crate::quest::JUDGE_TOOL,
        json!({"verdict": "ongoing", "reason": "not yet"}),
    ));
    game.turn(&client, "kick the elm door", &mut |_| {})
        .unwrap();
    let second = game
        .world()
        .last_event(EventKind::Input)
        .unwrap()
        .unwrap()
        .seq;
    let calls = game.world().turn_calls(second).unwrap();
    assert_eq!(roles(&calls), ["interpret", "narrator", "judge"]);
    assert_eq!(calls[0].call.model, "claude-haiku-5-5");
    assert!((calls[0].call.cost_usd.unwrap() - (0.00009 + 0.00002)).abs() < 1e-12);
    assert_eq!(
        calls[1].call.response["content"][0]["text"],
        "The elm door gives way."
    );

    // A tick is recorded against the turn before it.
    client.push(Response::canned_text("Nothing stirs."));
    game.tick(&client).unwrap();
    let calls = game.world().turn_calls(second).unwrap();
    assert_eq!(roles(&calls), ["interpret", "narrator", "judge", "tick"]);

    // Each system prompt is stored once however often it was sent.
    let all = game.world().calls().unwrap();
    let mut hashes: Vec<_> = all.iter().filter_map(|c| c.system_hash.clone()).collect();
    hashes.sort();
    hashes.dedup();
    let director = all.iter().filter(|c| c.call.role == "director").count();
    assert!(director >= 5);
    assert_eq!(
        hashes.len(),
        6,
        "bible, director, interpret, narrator, judge, tick"
    );

    // Records aren't events: replay is unaffected and copies none.
    let replayed = game.world().replay_in_memory().unwrap();
    assert_eq!(replayed.state().unwrap(), game.world().state().unwrap());
    assert!(replayed.calls().unwrap().is_empty());
    // A fork at the first turn's last event keeps the records up to it.
    let branch = game.world().fork_in_memory(second - 1).unwrap();
    assert_eq!(branch.calls().unwrap().len(), early.len() + 2);
}

#[test]
fn recording_can_be_turned_off() {
    let client = ScriptedClient::new([bible_call(), Response::canned_text("")]);
    let mut game = Game::new(World::open_in_memory().unwrap());
    game.set_recording(false);
    game.create_bible(&client, "A drowned abbey.").unwrap();
    assert_eq!(client.remaining(), 0);
    assert!(game.world().calls().unwrap().is_empty());
}

#[test]
fn calls_made_before_a_turn_fails_are_still_recorded() {
    let (mut game, client) = begun();
    let before = game.world().calls().unwrap().len();
    // The director makes one call, then the script runs dry.
    client.push(Response::canned_tool_use(
        "q",
        "query_entity",
        json!({"id": 1}),
    ));
    assert!(game.turn(&client, "look", &mut |_| {}).is_err());
    let calls = game.world().calls().unwrap();
    assert_eq!(calls.len(), before + 1);
    assert_eq!(calls.last().unwrap().call.role, "director");
}
