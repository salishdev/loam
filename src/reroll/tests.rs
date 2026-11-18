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
            mark: Mark::Plain,
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
            mark: Mark::Plain,
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

mod surgery_tests {
    use super::super::surgery::{self, Spec as SurgerySpec, Surgery, Target};
    use super::*;
    use crate::llm::ContentBlock;

    fn first_user_text(req: &crate::llm::Request) -> String {
        match &req.messages[0].content[0] {
            ContentBlock::Text { text } => text.clone(),
            other => panic!("expected text, got {other:?}"),
        }
    }

    fn operate(game: &Game, surgery: &Surgery, scripts: &[ScriptedClient]) -> Reroll {
        surgery::run(
            game.world(),
            surgery,
            game.brains(),
            &|n| &scripts[n - 1] as &(dyn LlmClient + Sync),
            &mut |_| {},
        )
        .unwrap()
    }

    #[test]
    fn spec_defaults_to_two_edited_variants_and_takes_reroll_knobs() {
        let s = SurgerySpec::parse("").unwrap();
        assert_eq!(
            (s.target, s.reroll.n, s.reroll.turn),
            (Target::Message, 2, None)
        );
        let s = SurgerySpec::parse("#7 system 4 director=haiku dice=fresh").unwrap();
        assert_eq!(s.target, Target::System);
        assert_eq!(
            (s.reroll.turn, s.reroll.n, s.reroll.fresh_dice),
            (Some(7), 4, true)
        );
        assert_eq!(s.reroll.director, Some(Model::Haiku));
        assert!(SurgerySpec::parse("8").unwrap_err().contains("1 to 7"));
        assert_eq!(SurgerySpec::parse("sideways").unwrap_err(), surgery::USAGE);
    }

    #[test]
    fn prepare_rebuilds_the_message_the_turn_sent() {
        let (mut game, client) = begun();
        let start = client.requests().len();
        client.push(Response::canned_text("You wait by the altar."));
        game.turn(&client, "light the candle", &mut |_| {}).unwrap();
        let sent = client.requests()[start].clone();
        let seq = xray::latest_turn(game.world()).unwrap().unwrap();
        // A later turn doesn't change what the earlier one is rebuilt from.
        client.push(Response::canned_text("Still waiting."));
        game.turn(&client, "wait", &mut |_| {}).unwrap();

        let spec = SurgerySpec::parse(&format!("#{seq}")).unwrap();
        let prepared = surgery::prepare(game.world(), &spec).unwrap();
        assert_eq!(prepared.turn_seq, seq);
        assert_eq!(prepared.input, "light the candle");
        assert_eq!(prepared.text(), first_user_text(&sent));
        let system = SurgerySpec::parse(&format!("#{seq} system")).unwrap();
        let prepared = surgery::prepare(game.world(), &system).unwrap();
        assert_eq!(Some(prepared.text()), sent.system.as_deref());

        let no_turn = surgery::prepare(game.world(), &SurgerySpec::parse("#1").unwrap());
        assert!(no_turn.unwrap_err().contains("isn't a turn's input"));
    }

    #[test]
    fn an_unchanged_or_empty_edit_cancels_without_calling_a_model() {
        let (game, _) = played();
        let calls = game.world().calls().unwrap().len();
        let spec = SurgerySpec::parse("").unwrap();
        let prepared = surgery::prepare(game.world(), &spec).unwrap();
        let text = prepared.text().to_owned();
        assert_eq!(surgery::apply(prepared.clone(), &text), None);
        assert_eq!(
            surgery::apply(prepared.clone(), &format!("{text}\n\n")),
            None
        );
        assert_eq!(surgery::apply(prepared.clone(), "  \n"), None);
        // Preparing made no call and wrote nothing.
        assert_eq!(game.world().calls().unwrap().len(), calls);
        assert!(game.world().rerolls().unwrap().is_empty());
        // An editor's trailing newline isn't part of the edit.
        let edit =
            surgery::apply(prepared, &format!("{}\n", text.replace("candle", "lamp"))).unwrap();
        assert!(!edit.edited.message.ends_with('\n'));
        assert!(
            edit.diff.contains("- ") && edit.diff.contains("lamp"),
            "{}",
            edit.diff
        );
    }

    #[test]
    fn edited_variants_send_the_edit_and_the_control_sends_the_original() {
        let (game, seq) = played();
        let state = game.world().state().unwrap();
        let spec = SurgerySpec::parse("").unwrap();
        let prepared = surgery::prepare(game.world(), &spec).unwrap();
        let original = prepared.prompt.clone();
        let edited_text = original.message.replace(
            "Player input:",
            "Engine note: the candle is cursed.\nPlayer input:",
        );
        let edit = surgery::apply(prepared, &edited_text).unwrap();
        let surgery = Surgery { spec, edit };
        let scripts: Vec<ScriptedClient> = ["A blue flame.", "It hisses.", "It catches."]
            .into_iter()
            .map(|t| ScriptedClient::new([Response::canned_text(t)]))
            .collect();
        let r = operate(&game, &surgery, &scripts);

        assert_eq!(r.head.turn_seq, seq);
        assert_eq!(r.head.n, 3);
        assert!(r.head.is_surgery());
        assert!(
            r.head
                .title()
                .starts_with(&format!("Surgery r{}", r.head.id))
        );
        assert!(
            r.head.title().contains("✂ message surgery"),
            "{}",
            r.head.title()
        );
        let marks: Vec<Mark> = r.variants.iter().map(|v| v.mark).collect();
        assert_eq!(marks, [Mark::Edited, Mark::Edited, Mark::Control]);
        assert_eq!(
            (r.head.mark(1), r.head.mark(3)),
            (Mark::Edited, Mark::Control)
        );
        for (i, script) in scripts.iter().enumerate() {
            let reqs = script.requests();
            assert_eq!(reqs.len(), 1, "variant {}: the director only", i + 1);
            let want = if i < 2 {
                &edited_text
            } else {
                &original.message
            };
            assert_eq!(&first_user_text(&reqs[0]), want, "variant {}", i + 1);
            assert_eq!(reqs[0].system.as_deref(), Some(original.system.as_str()));
        }
        assert!(variant_heading(&r.variants[0]).starts_with("Variant 1 ✂ ·"));
        assert!(variant_heading(&r.variants[2]).starts_with("Variant 3 (control) ·"));

        // The knobs record the surgery and its diff; the save is untouched
        // but for call records and the row.
        let rows = game.world().rerolls().unwrap();
        let knobs = &rows.last().unwrap().knobs;
        assert_eq!(knobs["surgery"]["target"], json!("message"));
        assert_eq!(knobs["surgery"]["control"], json!(3));
        assert!(
            knobs["surgery"]["diff"]
                .as_str()
                .unwrap()
                .contains("+ Engine note: the candle is cursed.")
        );
        assert_eq!(game.world().state().unwrap(), state);
        let overview = overview(&r.head, &r.variants).join("\n");
        assert!(overview.contains("✂ edit:\n  @@ …"), "{overview}");
        let listed = list(game.world()).unwrap();
        assert!(listed.contains("✂ message surgery") && listed.contains("✂ edit:"));
        // Edited variants can be adopted like any other.
        let dir = std::env::temp_dir().join(format!("loam-surgery-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("adopted.db");
        let _ = std::fs::remove_file(&path);
        r.adopt(1, &path).unwrap();
        let adopted = World::open(&path).unwrap();
        assert_eq!(
            adopted
                .last_event(EventKind::Narration)
                .unwrap()
                .unwrap()
                .payload["text"],
            json!("A blue flame.")
        );
        drop(adopted);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn system_surgery_edits_the_system_prompt_only() {
        let (game, _) = played();
        let spec = SurgerySpec::parse("system 1").unwrap();
        let prepared = surgery::prepare(game.world(), &spec).unwrap();
        let original = prepared.prompt.clone();
        let edit = surgery::apply(prepared, "You are a terse director.").unwrap();
        let surgery = Surgery { spec, edit };
        let scripts: Vec<ScriptedClient> = ["Lit.", "It catches."]
            .into_iter()
            .map(|t| ScriptedClient::new([Response::canned_text(t)]))
            .collect();
        let r = operate(&game, &surgery, &scripts);
        assert_eq!(r.head.n, 2);
        let edited = &scripts[0].requests()[0];
        assert_eq!(edited.system.as_deref(), Some("You are a terse director."));
        assert_eq!(first_user_text(edited), original.message);
        let control = &scripts[1].requests()[0];
        assert_eq!(control.system.as_deref(), Some(original.system.as_str()));
        assert!(r.head.title().contains("✂ system surgery"));
    }
}

#[test]
fn word_diff_shows_changed_words_with_context() {
    use super::surgery::word_diff;
    let d = word_diff(
        "the quick brown fox jumps over the lazy dog",
        "the quick red fox jumps over the dog",
    );
    assert_eq!(
        d,
        "@@ …the quick\n- brown fox jumps over the lazy\n+ red fox jumps over the"
    );
    let d = word_diff(
        "one two three four five six seven eight nine ten eleven twelve",
        "zero one two three four five six seven eight nine ten eleven",
    );
    assert_eq!(
        d,
        "@@ (start)\n+ zero\n@@ …six seven eight nine ten eleven\n- twelve"
    );
    assert_eq!(word_diff("same", "same"), "");
    assert_eq!(word_diff("a b", "a\nb"), "(whitespace only)");
}

#[test]
fn the_plain_report_of_a_surgery_shows_the_edit_and_marks() {
    let head = Head {
        id: 5,
        turn_seq: 9,
        input: "kick the door".into(),
        n: 2,
        knobs: json!({"surgery": {"target": "system", "control": 2, "diff": "@@ …be\n- kind\n+ cruel"}}),
        dice: "same".into(),
    };
    let variant = |n: usize, text: &str| VariantView {
        n,
        mark: head.mark(n),
        result: Ok(Played {
            narration: text.into(),
            route: "director".into(),
            facts: vec![],
        }),
        calls: 1,
        cost_usd: 0.001,
    };
    let text = report_text(
        &head,
        &[variant(1, "It splinters."), variant(2, "It holds.")],
    );
    let top: Vec<&str> = text.lines().take(6).collect();
    assert_eq!(
        top,
        [
            "Surgery r5 of turn #9 \"kick the door\" · 2 variants · ✂ system surgery · same dice",
            "total $0.002",
            "✂ edit:",
            "  @@ …be",
            "  - kind",
            "  + cruel",
        ]
    );
    assert!(
        text.contains("── Variant 1 ✂ · director · 1 call"),
        "{text}"
    );
    assert!(
        text.contains("── Variant 2 (control) · director · 1 call"),
        "{text}"
    );
}
