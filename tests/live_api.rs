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

/// The live director writes a stat sheet with the bible, every stat with
/// its advancement charter. Prints the sheet and its rules.
#[test]
#[ignore = "calls the live Claude API; needs ANTHROPIC_API_KEY"]
fn seed_builds_a_stat_sheet() {
    use loam::game::Game;
    use loam::world::World;

    let client = AnthropicClient::from_env().expect("set ANTHROPIC_API_KEY to run live tests");
    let mut game = Game::new(World::open_in_memory().unwrap());
    game.create_bible(
        &client,
        "A haunted salt mine under a dying town. Slow dread, small hopes.",
    )
    .unwrap();
    let stats = game.stats().unwrap();
    assert!(!stats.is_empty());
    for line in loam::stats::slice_lines(game.world()).unwrap() {
        println!("{line}");
    }
    let rules = loam::stats::stat_rules(game.world()).unwrap();
    for s in &stats {
        assert!(rules.iter().any(|r| matches!(
            &r.hook,
            Some(loam::stats::StatHook::Advancement { stat, .. }) if stat == &s.name
        )));
    }
}

/// Wraps the real client and keeps every call's usage.
struct Recording {
    inner: AnthropicClient,
    usages: std::cell::RefCell<Vec<loam::llm::Usage>>,
}

impl loam::llm::LlmClient for Recording {
    fn send(
        &self,
        request: &Request,
        on_text: &mut dyn FnMut(&str),
    ) -> Result<loam::llm::Response, loam::llm::LlmError> {
        let response = self.inner.send(request, on_text)?;
        self.usages.borrow_mut().push(response.usage);
        Ok(response)
    }
}

/// A tool loop with a system prompt over the 512-token minimum: the first
/// call writes the cache, every later call in the loop reads it back.
fn cache_hits_in_tool_loop(model: Model) {
    let client = Recording {
        inner: AnthropicClient::from_env().expect("set ANTHROPIC_API_KEY to run live tests"),
        usages: Default::default(),
    };
    let mut system = String::from(
        "You are a careful assistant in a test harness. Follow the user's steps exactly.\n\n",
    );
    for i in 1..=60 {
        system.push_str(&format!(
            "House rule {i}: keep replies short, never invent tool results, and call tools \
             one at a time, waiting for each result before the next call.\n"
        ));
    }
    let request = Request::new(
        model,
        vec![Message::user(
            "Call get_part with part 1. After its result, call get_part with part 2. \
             Then reply with just the two parts joined, part 1 first.",
        )],
    )
    .system(system)
    .max_tokens(2_000)
    .tools(vec![ToolDefinition::new(
        "get_part",
        "Returns one part of a code.",
        json!({"type": "object", "properties": {"part": {"type": "integer"}},
               "required": ["part"], "additionalProperties": false}),
    )]);

    let outcome =
        run_tool_loop(
            &client,
            request,
            6,
            &mut |_| {},
            &mut |call| match call.input["part"].as_i64() {
                Some(1) => ToolOutput::Ok("48".into()),
                Some(2) => ToolOutput::Ok("15".into()),
                _ => ToolOutput::Err("part must be 1 or 2".into()),
            },
        )
        .expect("tool loop should succeed");

    let usages = client.usages.into_inner();
    for (i, u) in usages.iter().enumerate() {
        println!("call {}: {u:?}", i + 1);
    }
    assert!(outcome.final_response.text().contains("4815"));
    assert!(usages.len() >= 2, "expected a multi-call loop");
    assert!(
        usages[0].cache_creation_input_tokens + usages[0].cache_read_input_tokens > 0,
        "first call should write (or, on a rerun, read) the cache"
    );
    for (i, u) in usages.iter().enumerate().skip(1) {
        assert!(
            u.cache_read_input_tokens > 0,
            "call {} read nothing from the cache",
            i + 1
        );
    }
}

#[test]
#[ignore = "calls the live Claude API; needs ANTHROPIC_API_KEY"]
fn haiku_tool_loop_reads_cache() {
    cache_hits_in_tool_loop(Model::Haiku);
}

#[test]
#[ignore = "calls the live Claude API; needs ANTHROPIC_API_KEY"]
fn sonnet_tool_loop_reads_cache() {
    cache_hits_in_tool_loop(Model::Sonnet);
}

/// A small live reroll: a short game, one turn, then that turn rerolled
/// twice with the director on Haiku. Prints the report; checks the save
/// was left alone and the variants' calls were recorded.
#[test]
#[ignore = "calls the live Claude API; needs ANTHROPIC_API_KEY"]
fn a_small_reroll_runs_two_variants() {
    use loam::game::Game;
    use loam::llm::LlmClient;
    use loam::reroll::{self, Progress, Spec};
    use loam::world::World;

    let client = AnthropicClient::from_env().expect("set ANTHROPIC_API_KEY to run live tests");
    let mut game = Game::new(World::open_in_memory().unwrap());
    game.create_bible(
        &client,
        "A lighthouse on a foggy rock; the lamp has gone out. Quiet, eerie, small-scale.",
    )
    .unwrap();
    game.begin(&client, &mut |_| {}).unwrap();
    game.turn(&client, "try to relight the lamp", &mut |_| {})
        .unwrap();
    let state = game.world().state().unwrap();
    let last = game.world().last_seq().unwrap();

    let spec = Spec::parse("2 director=haiku").unwrap();
    let r = reroll::run(
        game.world(),
        &spec,
        game.brains(),
        &|_| &client as &(dyn LlmClient + Sync),
        &mut |p| {
            if let Progress::Variant(v) = p {
                println!("[{}]", reroll::variant_heading(v));
            }
        },
    )
    .unwrap();
    println!("\n{}", reroll::report_text(&r.head, &r.variants));
    assert_eq!(r.variants.len(), 2);
    assert!(
        r.variants.iter().all(|v| v.result.is_ok()),
        "{:?}",
        r.variants
    );
    assert_eq!(game.world().state().unwrap(), state);
    assert_eq!(game.world().last_seq().unwrap(), last);
    for n in 1..=2 {
        let calls = game.world().variant_calls(r.head.id, n).unwrap();
        assert!(calls.iter().any(|c| c.call.model == Model::Haiku.id()));
    }
}
