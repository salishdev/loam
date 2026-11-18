use serde_json::json;

use super::*;
use crate::game::tests::begun;
use crate::llm::{Response, ScriptedClient};

/// The candle's id in the opening room.
fn candle(world: &World) -> EntityId {
    world
        .state()
        .unwrap()
        .entities
        .iter()
        .find(|e| e.name == "tallow candle")
        .unwrap()
        .id
}

fn light(candle: EntityId) -> Response {
    Response::canned_tool_use(
        "p",
        "apply_patch",
        json!({"ops": [{"op": "set_prop", "entity": candle, "key": "lit", "value": true}]}),
    )
}

fn check() -> Response {
    Response::canned_tool_use(
        "c",
        "check",
        json!({"stat": "nerve", "difficulty": 12, "reason": "steady hands"}),
    )
}

fn pass_time(minutes: u64) -> Response {
    Response::canned_tool_use(
        "t",
        "pass_time",
        json!({"minutes": minutes, "reason": "a long vigil"}),
    )
}

/// A begun game with one played turn (a roll, then text); returns the
/// game and that turn's input event.
fn played() -> (Game, i64) {
    let (mut game, client) = begun();
    client.push(check());
    client.push(Response::canned_text("You wait by the altar."));
    game.turn(&client, "light the candle", &mut |_| {}).unwrap();
    let seq = xray::latest_turn(game.world()).unwrap().unwrap();
    (game, seq)
}

fn rolls(world: &World, after: i64) -> Vec<u64> {
    world
        .events_since(after)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EventKind::Roll)
        .map(|e| e.payload["seed"].as_u64().unwrap())
        .collect()
}

/// Runs `spec` with one script per variant; returns the reroll and the
/// progress it reported.
fn reroll(game: &Game, spec: &Spec, scripts: Vec<ScriptedClient>) -> (Reroll, Vec<String>) {
    let mut seen = Vec::new();
    let r = run(
        game.world(),
        spec,
        game.brains(),
        &|n| &scripts[n - 1] as &(dyn LlmClient + Sync),
        &mut |p| match p {
            Progress::Started(h) => seen.push(format!("start r{}", h.id)),
            Progress::Variant(v) => seen.push(format!("variant {}", v.n)),
        },
    )
    .unwrap();
    (r, seen)
}

#[test]
fn spec_parses_knobs_in_any_order_and_refuses_nonsense() {
    assert_eq!(Spec::parse("").unwrap(), Spec::default());
    let s = Spec::parse("5 #12 director=haiku effort=high dice=fresh").unwrap();
    assert_eq!(
        s,
        Spec {
            turn: Some(12),
            n: 5,
            director: Some(Model::Haiku),
            effort: Some(Effort::High),
            fresh_dice: true,
        }
    );
    assert_eq!(s.knobs(), json!({"director": "haiku", "effort": "high"}));
    assert_eq!(s.dice(), "fresh");
    assert!(Spec::parse("9").unwrap_err().contains("1 to 8"));
    assert!(Spec::parse("0").is_err());
    assert!(
        Spec::parse("director=gpt")
            .unwrap_err()
            .contains("Unknown model")
    );
    assert_eq!(Spec::parse("sideways").unwrap_err(), USAGE);
    let brains = Spec::parse("director=opus effort=max")
        .unwrap()
        .brains(&Brains::default());
    let director = brains.get("director").unwrap();
    assert_eq!(
        (director.model.clone(), director.effort),
        (Model::Opus, Some(Effort::Max))
    );
    assert_eq!(brains.get("narrator"), Brains::default().get("narrator"));
}

#[test]
fn differing_variants_give_a_grouped_summary_and_leave_the_save_alone() {
    let (game, seq) = played();
    let world = game.world();
    let id = candle(world);
    let state = world.state().unwrap();
    let last = world.last_seq().unwrap();
    let scripts = vec![
        ScriptedClient::new([
            light(id),
            pass_time(180),
            Response::canned_text("It flares."),
        ]),
        ScriptedClient::new([light(id), Response::canned_text("It catches.")]),
        ScriptedClient::new([Response::canned_text("Your hands shake.")]),
    ];
    let (r, seen) = reroll(&game, &Spec::default(), scripts);

    assert_eq!(seen[0], format!("start r{}", r.head.id));
    assert_eq!(seen[1], "variant 1", "variant 1 runs first: {seen:?}");
    assert_eq!(seen.len(), 4);
    assert_eq!(r.head.turn_seq, seq);
    assert_eq!(r.head.input, "light the candle");

    let s = r.summary();
    assert_eq!(s.routes, vec![("director".to_string(), vec![1, 2, 3])]);
    assert_eq!(
        s.facts,
        vec![
            ("tallow candle.lit: false → true".to_string(), vec![1, 2]),
            (NO_CHANGE.to_string(), vec![3]),
        ]
    );
    let text = summary_text(&s, 3);
    assert_eq!(
        text,
        vec![
            "route: director 3/3",
            "2/3  tallow candle.lit: false → true  (variants 1, 2)",
            "1/3  nothing changed  (variant 3)",
        ]
    );
    // Narration and engine facts per variant.
    let v1 = r.variants[0].result.as_ref().unwrap();
    assert_eq!(v1.narration, "It flares.");
    assert!(
        v1.facts
            .iter()
            .any(|f| f.engine && f.text.starts_with("time +180 min"))
    );

    // No tick, even though three hours passed in variant 1.
    let v1_world = r.world(1).unwrap();
    assert!(
        v1_world
            .events_since(seq - 1)
            .unwrap()
            .iter()
            .all(|e| e.kind != EventKind::Tick)
    );

    // The save played is unchanged but for records.
    assert_eq!(world.state().unwrap(), state);
    assert_eq!(world.last_seq().unwrap(), last);
    let rows = world.rerolls().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].summary.as_deref(), Some(text.join("\n").as_str()));
    assert_eq!(world.variant_calls(r.head.id, 1).unwrap().len(), 3);
    assert_eq!(world.variant_calls(r.head.id, 3).unwrap().len(), 1);
    // The turn's own calls leave the variants out.
    assert_eq!(world.turn_calls(seq).unwrap().len(), 2);
}

#[test]
fn same_dice_roll_what_the_turn_rolled_and_fresh_dice_reseed() {
    let (game, seq) = played();
    let original = rolls(game.world(), seq - 1);
    assert_eq!(original.len(), 1);
    let scripts = || {
        (0..2)
            .map(|_| ScriptedClient::new([check(), Response::canned_text("Steady.")]))
            .collect::<Vec<_>>()
    };
    let (same, _) = reroll(
        &game,
        &Spec {
            n: 2,
            ..Spec::default()
        },
        scripts(),
    );
    for n in 1..=2 {
        assert_eq!(rolls(same.world(n).unwrap(), seq - 1), original);
    }
    let fresh_spec = Spec {
        n: 2,
        fresh_dice: true,
        ..Spec::default()
    };
    let (fresh, _) = reroll(&game, &fresh_spec, scripts());
    let a = rolls(fresh.world(1).unwrap(), seq - 1);
    let b = rolls(fresh.world(2).unwrap(), seq - 1);
    assert_ne!(a, original);
    assert_ne!(b, original);
    assert_ne!(a, b);
    assert_eq!(fresh.head.dice, "fresh");
    let listed = list(game.world()).unwrap();
    assert!(
        listed.contains("same dice") && listed.contains("fresh dice"),
        "{listed}"
    );
}

#[test]
fn a_failed_variant_is_reported_and_the_rest_still_count() {
    let (game, _) = played();
    let scripts = vec![
        ScriptedClient::new([Response::canned_text("Fine.")]),
        ScriptedClient::new([]),
    ];
    let (r, _) = reroll(
        &game,
        &Spec {
            n: 2,
            ..Spec::default()
        },
        scripts,
    );
    assert!(r.variants[1].result.is_err());
    assert!(r.world(2).is_none());
    let s = r.summary();
    assert_eq!(s.failed, vec![2]);
    assert!(summary_text(&s, 2).contains(&"failed: variant 2".to_string()));
    let dir = std::env::temp_dir().join(format!("loam-reroll-fail-{}", std::process::id()));
    assert!(
        r.adopt(2, &dir.join("x.db"))
            .unwrap_err()
            .contains("failed")
    );
    assert!(
        r.adopt(3, &dir.join("x.db"))
            .unwrap_err()
            .contains("no variant 3")
    );
}

#[test]
fn rerolling_needs_a_turn() {
    let (game, _) = begun();
    let err = run(
        game.world(),
        &Spec::default(),
        game.brains(),
        &|_| unreachable!(),
        &mut |_| {},
    )
    .unwrap_err();
    assert_eq!(err, "No turn to reroll yet.");
    let err = run(
        game.world(),
        &Spec {
            turn: Some(1),
            ..Spec::default()
        },
        game.brains(),
        &|_| unreachable!(),
        &mut |_| {},
    )
    .unwrap_err();
    assert!(err.contains("isn't a turn's input"), "{err}");
}

#[test]
fn knobs_reach_the_variants_requests() {
    let (game, _) = played();
    let scripts = vec![ScriptedClient::new([Response::canned_text("Hm.")])];
    let spec = Spec::parse("1 director=haiku effort=low").unwrap();
    let (r, _) = reroll(&game, &spec, scripts);
    let calls = game.world().variant_calls(r.head.id, 1).unwrap();
    assert_eq!(calls[0].call.model, Model::Haiku.id());
    assert_eq!(calls[0].call.effort.as_deref(), Some("low"));
    assert!(
        r.head
            .title()
            .contains("director haiku · effort low · same dice")
    );
}

#[test]
fn an_adopted_variant_is_a_branch_that_replays_to_its_state() {
    use crate::branches::tests::{saved_game, temp_dir};
    let dir = temp_dir("reroll-adopt");
    let main = saved_game(&dir, "mill.db");
    let mut game = Game::new(World::open(&main).unwrap());
    let client = ScriptedClient::new([Response::canned_text("You wait.")]);
    game.turn(&client, "light the candle", &mut |_| {}).unwrap();
    let seq = xray::latest_turn(game.world()).unwrap().unwrap();
    // A later turn, which the adopted branch leaves behind.
    client.push(Response::canned_text("Reeds whisper."));
    game.turn(&client, "listen", &mut |_| {}).unwrap();
    let before = game.world().state().unwrap();
    let last = game.world().last_seq().unwrap();

    let id = candle(game.world());
    let scripts = vec![
        ScriptedClient::new([Response::canned_text("Nothing.")]),
        ScriptedClient::new([light(id), Response::canned_text("It flares.")]),
    ];
    let spec = Spec {
        turn: Some(seq),
        n: 2,
        ..Spec::default()
    };
    let (r, _) = reroll(&game, &spec, scripts);
    let path = crate::branches::next_path(&main);
    r.adopt(2, &path).unwrap();
    assert!(r.adopt(2, &path).unwrap_err().contains("already exists"));

    let branch = World::open(&path).unwrap();
    let variant = r.world(2).unwrap();
    assert_eq!(branch.state().unwrap(), variant.state().unwrap());
    assert_eq!(
        branch.replay_in_memory().unwrap().state().unwrap(),
        variant.state().unwrap()
    );
    assert_eq!(branch.forked_at().unwrap(), Some(seq - 1));
    assert_eq!(
        branch.parent().unwrap().map(std::path::PathBuf::from),
        Some(main.clone())
    );
    assert_eq!(
        branch.label().unwrap().as_deref(),
        Some(&*format!("reroll r{}.2", r.head.id))
    );
    assert_eq!(branch.lineage().unwrap(), game.world().lineage().unwrap());
    // The variant's calls are the branch's record of the turn.
    assert_eq!(branch.turn_calls(seq).unwrap().len(), 2);
    let log = crate::inspect::log(&branch, "narration").unwrap();
    assert!(
        log.contains("It flares.") && !log.contains("Reeds whisper"),
        "{log}"
    );
    // The original save is untouched.
    assert_eq!(game.world().state().unwrap(), before);
    assert_eq!(game.world().last_seq().unwrap(), last);
    let tree = crate::branches::scan(&main).unwrap();
    assert_eq!(tree.len(), 2);
    drop((branch, game, r));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn the_plain_report_stacks_the_variants_under_the_summary() {
    let head = Head {
        id: 4,
        turn_seq: 9,
        input: "kick the door".into(),
        n: 2,
        knobs: json!({"director": "haiku"}),
        dice: "same".into(),
    };
    let fact = |t: &str| Fact::new(t);
    let variants = vec![
        VariantView {
            n: 2,
            result: Ok(Played {
                narration: "It holds.".into(),
                route: "director".into(),
                facts: vec![],
            }),
            calls: 1,
            cost_usd: 0.002,
        },
        VariantView {
            n: 1,
            result: Ok(Played {
                narration: "The door bursts.".into(),
                route: "director".into(),
                facts: vec![
                    fact("door.broken: false → true"),
                    Fact::engine("time +10 min (default)"),
                ],
            }),
            calls: 2,
            cost_usd: 0.004,
        },
    ];
    let text = report_text(&head, &variants);
    let expected = "\
Reroll r4 of turn #9 \"kick the door\" · 2 variants · director haiku · same dice
total $0.006
route: director 2/2
1/2  door.broken: false → true  (variant 1)
1/2  nothing changed  (variant 2)

── Variant 1 · director · 2 calls · $0.004
The door bursts.

  door.broken: false → true

── Variant 2 · director · 1 call · $0.002
It holds.

  (nothing changed)

:reroll show <n> reads a variant; :adopt <n> plays it as a new branch; :context r4.<n> lists its calls.";
    assert_eq!(text, expected);
    let shown = variant_text(&variants[1]);
    assert!(
        shown.contains("time +10 min"),
        "full diff shows engine facts: {shown}"
    );
    // Part way through, the overview says how far it got.
    assert!(overview(&head, &variants[..1])[1].contains("1/2 done"));
}
