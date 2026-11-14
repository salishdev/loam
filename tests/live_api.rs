//! Live Claude API checks. They cost money and need `ANTHROPIC_API_KEY`, so
//! they are ignored by default:
//!
//! ```sh
//! ANTHROPIC_API_KEY=... cargo test --test live_api -- --ignored --nocapture
//! ```

use loam::llm::{
    AnthropicClient, Message, Model, Request, StopReason, ToolDefinition, ToolOutput, run_tool_loop,
};
use serde_json::json;

fn round_trip(model: Model) {
    let client = AnthropicClient::from_env().expect("set ANTHROPIC_API_KEY to run live tests");
    let secret = "7319";
    let request = Request::new(
        model,
        vec![Message::user(
            "What is the secret number? Use the tool, then reply with just the number.",
        )],
    )
    .max_tokens(2_000)
    .tools(vec![ToolDefinition::new(
        "get_secret_number",
        "Returns the secret number. Takes no arguments.",
        json!({"type": "object", "properties": {}, "additionalProperties": false}),
    )]);

    let mut tool_calls = 0;
    let mut streamed = String::new();
    let outcome = run_tool_loop(
        &client,
        request,
        4,
        &mut |t| streamed.push_str(t),
        &mut |call| {
            assert_eq!(call.name, "get_secret_number");
            tool_calls += 1;
            ToolOutput::Ok(secret.into())
        },
    )
    .expect("tool loop should succeed");

    println!(
        "model={} calls={} text={:?} usage={:?}",
        outcome.final_response.model,
        outcome.calls,
        outcome.final_response.text(),
        outcome.final_response.usage
    );
    assert!(tool_calls >= 1, "model never called the tool");
    assert_eq!(
        outcome.final_response.stop_reason,
        Some(StopReason::EndTurn)
    );
    assert!(outcome.final_response.text().contains(secret));
    assert!(
        streamed.contains(secret),
        "answer should have arrived through the text stream"
    );
}

#[test]
#[ignore = "calls the live Claude API; needs ANTHROPIC_API_KEY"]
fn haiku_tool_use_round_trip() {
    round_trip(Model::Haiku);
}

#[test]
#[ignore = "calls the live Claude API; needs ANTHROPIC_API_KEY"]
fn sonnet_tool_use_round_trip() {
    round_trip(Model::Sonnet);
}

/// The real Haiku judge rules a plainly met goal complete through its tool.
#[test]
#[ignore = "calls the live Claude API; needs ANTHROPIC_API_KEY"]
fn haiku_judges_a_met_goal_complete() {
    use loam::quest::{self, Status, Verdict};
    use loam::world::{Op, Patch, World};

    let client = AnthropicClient::from_env().expect("set ANTHROPIC_API_KEY to run live tests");
    let mut world = World::open_in_memory().unwrap();
    let lamp = world
        .apply_patch(&Patch::new(vec![Op::Create {
            handle: None,
            name: "brass lamp".into(),
            kind: "item".into(),
            location: None,
            props: Default::default(),
        }]))
        .unwrap()
        .created[0];
    let q = quest::spawn(
        &mut world,
        "Light in the dark",
        "The cellar is pitch black.",
        &[],
    )
    .unwrap();
    quest::reveal_goal(&mut world, q, "The brass lamp is lit.", &[lamp]).unwrap();
    let start = world.last_seq().unwrap();
    world
        .apply_patch(&Patch::new(vec![Op::SetProp {
            entity: lamp.into(),
            key: "lit".into(),
            value: json!(true),
        }]))
        .unwrap();
    let judgments = quest::judge_turn(
        &mut world,
        &client,
        start,
        Some("light the lamp"),
        "The wick catches and the brass lamp glows.",
    )
    .unwrap();
    println!("{judgments:?}");
    assert_eq!(judgments.len(), 1);
    assert_eq!(judgments[0].verdict, Verdict::Complete);
    assert_eq!(
        quest::quest(&world, q).unwrap().unwrap().status,
        Status::Complete
    );
}

/// A short live game: seed, opening, one step out and back, then a world
/// tick. Prints what the tick did; checks it left the player's room alone.
#[test]
#[ignore = "calls the live Claude API; needs ANTHROPIC_API_KEY"]
fn world_tick_advances_off_screen() {
    use loam::game::Game;
    use loam::world::{EventKind, World};

    let client = AnthropicClient::from_env().expect("set ANTHROPIC_API_KEY to run live tests");
    let mut game = Game::new(World::open_in_memory().unwrap());
    game.create_bible(
        &client,
        "A fishing village where the smugglers and the lighthouse keeper are at war. \
         Wry, salty, small-scale.",
    )
    .unwrap();
    let mut print = |t: &str| print!("{t}");
    game.begin(&client, &mut print).unwrap();
    for input in [
        "go through the most interesting exit",
        "go back the way I came",
    ] {
        println!("\n> {input}");
        game.turn(&client, input, &mut print).unwrap();
    }
    let before = game.world().state().unwrap();
    let tick = game.tick(&client).unwrap();
    println!("\n\n[tick] {} ({} calls)", tick.summary, tick.calls);
    for c in &tick.changes {
        println!("  - {} rooms={:?}", c.note, c.rooms);
    }
    let after = game.world().state().unwrap();
    assert!(!tick.summary.is_empty());
    let ticks = game
        .world()
        .search_events("", &[EventKind::Tick], 5)
        .unwrap();
    assert_eq!(ticks.len(), 1);
    let here = ticks[0].payload["room"].as_i64();
    assert!(
        tick.changes
            .iter()
            .all(|c| !c.rooms.iter().any(|r| Some(*r) == here)),
        "a tick touched the player's room"
    );
    if tick.changes.is_empty() {
        assert_eq!(before, after);
    }
    println!("\n{}", loam::game::local_slice(game.world()).unwrap());
}
