use serde_json::{Value, json};

use super::*;
use crate::inventory::{DEFAULT_SLOTS, Inventory};
use crate::rules::{self, Amendment, RuleClass, Scope};
use crate::world::{CARRIER_PROP, Patch};

const SEED: u64 = 42;

struct Fixture {
    world: World,
    player: EntityId,
    room: EntityId,
}

/// A player standing in a hall, with no sheet yet.
fn bare() -> Fixture {
    let mut world = World::open_in_memory().unwrap();
    let ops: Vec<Op> = serde_json::from_value(json!([
        {"op": "create", "handle": "hall", "name": "Hall", "kind": "room"},
        {"op": "create", "handle": "you", "name": "you", "kind": "player", "location": "hall",
         "props": {CARRIER_PROP: true}}
    ]))
    .unwrap();
    let a = world.apply_engine_patch(&Patch::new(ops)).unwrap();
    Fixture {
        player: a.handles["you"],
        room: a.handles["hall"],
        world,
    }
}

fn specs(v: Value) -> Vec<StatSpec> {
    serde_json::from_value(v).unwrap()
}

/// [`bare`] with the sample sheet written.
fn fixture() -> Fixture {
    let mut f = bare();
    create_sheet(&mut f.world, &specs(sample_sheet()), SEED)
        .unwrap()
        .unwrap();
    f
}

fn value(world: &World, stat: &str) -> i64 {
    sheet(world).unwrap()[stat].value
}

fn advancement_of(world: &World, stat: &str) -> RuleId {
    stat_rules(world)
        .unwrap()
        .into_iter()
        .find(|r| matches!(&r.hook, Some(StatHook::Advancement { stat: s, .. }) if s == stat))
        .unwrap()
        .id
}

fn lower(world: &mut World, stat: &str, by: i64) -> Changes {
    adjust_stat(world, stat, -by, "test", None)
        .unwrap()
        .unwrap()
}

#[test]
fn the_sheet_ships_with_its_rules() {
    let f = fixture();
    let s = sheet(&f.world).unwrap();
    assert_eq!(s.keys().collect::<Vec<_>>(), ["brawn", "nerve"]);
    assert_eq!(
        (s["nerve"].value, s["nerve"].min, s["nerve"].max),
        (3, 0, 5)
    );
    let hooks: Vec<(String, &str)> = stat_rules(&f.world)
        .unwrap()
        .into_iter()
        .map(|r| {
            assert_eq!(r.class, RuleClass::Stat);
            let kind = match r.hook.as_ref().unwrap() {
                StatHook::Bounds { .. } => "bounds",
                StatHook::Threshold { .. } => "threshold",
                StatHook::Capacity { .. } => "capacity",
                StatHook::Advancement { .. } => "advancement",
            };
            (r.hook.as_ref().unwrap().stat().to_owned(), kind)
        })
        .collect();
    assert_eq!(
        hooks,
        [
            ("nerve".to_owned(), "bounds"),
            ("nerve".to_owned(), "threshold"),
            ("nerve".to_owned(), "advancement"),
            ("brawn".to_owned(), "bounds"),
            ("brawn".to_owned(), "capacity"),
            ("brawn".to_owned(), "advancement"),
        ]
    );
    // The interpreter never sees stat rules.
    assert!(rules::active_world(&f.world).unwrap().is_empty());
    // The slice shows the sheet with its charter.
    let lines = slice_lines(&f.world).unwrap();
    assert!(lines[1].starts_with("- nerve 3 [0..5]"), "{lines:?}");
    assert!(lines[1].contains("nerve falls to 0 or below"), "{lines:?}");
    assert!(
        lines[1].contains("facing a horror without fleeing (+1)"),
        "{lines:?}"
    );
}

#[test]
fn an_invalid_sheet_writes_nothing() {
    let refused = |patch: Value| {
        let mut f = bare();
        let mut sheet = sample_sheet();
        for (k, v) in patch.as_object().unwrap() {
            sheet[0][k] = v.clone();
        }
        let err = create_sheet(&mut f.world, &specs(sheet), SEED)
            .unwrap()
            .unwrap_err();
        assert!(rules::all(&f.world).unwrap().is_empty());
        assert!(super::sheet(&f.world).unwrap().is_empty());
        err
    };
    assert!(refused(json!({"advancement": []})).contains("advancement"));
    assert!(refused(json!({"start": 9})).contains("start"));
    assert!(refused(json!({"min": 5})).contains("min"));
    assert!(refused(json!({"name": "Nerve"})).contains("lowercase"));
    assert!(refused(json!({"name": "brawn"})).contains("twice"));
    assert!(
        refused(json!({"capacity": {"capacity": "carry_limit", "base": 1, "per_point": 1}}))
            .contains("only one stat")
    );
    assert!(
        refused(
            json!({"thresholds": [{"at": 1, "when": "falls_to", "outcome": "x",
                                       "check": {"stat": "luck", "difficulty": 10}}]})
        )
        .contains("unknown stat")
    );
    assert!(
        refused(
            json!({"thresholds": [{"at": 1, "when": "falls_to", "outcome": "x",
            "effects": [{"op": "set_prop", "entity": "player", "key": "_stats", "value": {}}]}]})
        )
        .contains("engine-owned")
    );
    assert!(
        refused(json!({"thresholds": [{"at": 9, "when": "falls_to", "outcome": "x"}]}))
            .contains("outside")
    );
}

#[test]
fn threshold_effects_cant_name_an_entity_by_id() {
    let mut f = bare();
    let ops: Vec<Op> = serde_json::from_value(json!([
        {"op": "create", "name": "gate", "kind": "gate", "location": f.room}
    ]))
    .unwrap();
    let gate = f
        .world
        .apply_engine_patch(&Patch::new(ops))
        .unwrap()
        .created[0];
    let main = json!([{"at": 1, "when": "falls_to", "outcome": "x",
        "effects": [{"op": "set_prop", "entity": gate, "key": "shut", "value": true}]}]);
    let branch = json!([{"at": 1, "when": "falls_to", "outcome": "x",
        "check": {"stat": "nerve", "difficulty": 10,
                  "failure": {"outcome": "y",
                              "effects": [{"op": "destroy", "entity": gate}]}}}]);
    for thresholds in [main, branch] {
        let mut sheet = sample_sheet();
        sheet[0]["thresholds"] = thresholds;
        let err = create_sheet(&mut f.world, &specs(sheet.clone()), SEED)
            .unwrap()
            .unwrap_err();
        assert!(err.contains(&format!("names #{gate}")), "{err}");
        assert!(rules::all(&f.world).unwrap().is_empty());
        // Mid-game too: add_stat plans its rules the same way.
        sheet[0]["name"] = json!("grit");
        let err = add_stat(&mut f.world, &specs(sheet)[0], SEED, "found grit")
            .unwrap()
            .unwrap_err();
        assert!(err.contains(&format!("names #{gate}")), "{err}");
        assert!(super::sheet(&f.world).unwrap().is_empty());
    }
}

#[test]
fn a_threshold_fires_once_per_crossing() {
    let mut f = fixture();
    // 3 -> 1: no threshold crossed.
    let c = lower(&mut f.world, "nerve", 2);
    assert_eq!(
        c.changed,
        [StatChange {
            stat: "nerve".into(),
            from: 3,
            to: 1
        }]
    );
    assert!(c.fired.is_empty());

    // 1 -> 0: the setback fires and its effects apply.
    let c = lower(&mut f.world, "nerve", 1);
    assert_eq!(c.fired.len(), 1, "{c:?}");
    assert!(c.fired[0].outcome.contains("nerve breaks"));
    assert_eq!(c.fired[0].error, None);
    let player = f.world.entity(f.player).unwrap().unwrap();
    assert_eq!(player.props["shaken"], json!(true));
    // The threshold's patch names its rule.
    let patch = f
        .world
        .events()
        .unwrap()
        .into_iter()
        .rev()
        .find(|e| e.kind == EventKind::Patch && e.payload.get("rule").is_some())
        .unwrap();
    assert_eq!(patch.payload["rule"]["id"], json!(c.fired[0].rule));
    assert_eq!(patch.payload["rule"]["threshold"], json!(true));

    // Already at the floor: clamped, nothing fires again.
    let c = lower(&mut f.world, "nerve", 3);
    assert_eq!((c.changed[0].from, c.changed[0].to), (0, 0));
    assert!(c.fired.is_empty());

    // Recovering and falling again crosses again.
    let rule = advancement_of(&f.world, "nerve");
    adjust_stat(&mut f.world, "nerve", 1, "stood firm", Some(rule))
        .unwrap()
        .unwrap();
    assert_eq!(lower(&mut f.world, "nerve", 1).fired.len(), 1);

    // Every change is a logged engine patch with its reason.
    let reasons: Vec<Value> = f
        .world
        .events()
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EventKind::Patch && e.payload.get("stat").is_some())
        .map(|e| e.payload["stat"]["reason"].clone())
        .collect();
    assert_eq!(
        reasons,
        [
            json!("test"),
            json!("test"),
            json!("stood firm"),
            json!("test")
        ]
    );
}

#[test]
fn thresholds_can_rise_chain_and_embed_checks() {
    let mut f = fixture();
    // dread rising to 3 costs 3 nerve (which crosses nerve's own threshold)
    // and rolls brawn: whatever the result, the threshold's chosen outcome
    // is reported.
    let spec: StatSpec = serde_json::from_value(json!({
        "name": "dread", "about": "the marsh's hold on you", "min": 0, "max": 5, "start": 0,
        "thresholds": [{"at": 3, "when": "rises_to", "outcome": "The marsh whispers your name.",
                        "adjust": [{"stat": "nerve", "by": -3}],
                        "check": {"stat": "brawn", "difficulty": 1,
                                  "failure": {"outcome": "You stumble."}}}],
        "advancement": [{"when": "lingering in the marsh after dark", "step": 3}]
    }))
    .unwrap();
    add_stat(&mut f.world, &spec, SEED, "the marsh noticed you")
        .unwrap()
        .unwrap();
    let rule = advancement_of(&f.world, "dread");
    let c = adjust_stat(&mut f.world, "dread", 3, "stayed out", Some(rule))
        .unwrap()
        .unwrap();
    let names: Vec<&str> = c.fired.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["dread rises to 3", "nerve falls to 0"]);
    // brawn 2 + d20 always beats 1.
    let roll = c.fired[0].roll.as_ref().unwrap();
    assert_eq!(roll.result, CheckResult::Success);
    assert_eq!(c.fired[0].outcome, "The marsh whispers your name.");
    assert_eq!(value(&f.world, "nerve"), 0);
    assert_eq!(value(&f.world, "dread"), 3);
}

#[test]
fn stats_and_their_rules_never_change() {
    let mut f = fixture();
    for r in stat_rules(&f.world).unwrap() {
        let amended = rules::amend(&mut f.world, r.id, Amendment::default(), "")
            .unwrap()
            .unwrap_err();
        assert!(amended.contains("stat rules never change"), "{amended}");
        let retired = rules::retire(&mut f.world, r.id, "").unwrap().unwrap_err();
        assert!(retired.contains("stat rules never change"), "{retired}");
    }
    // A stat can't be redefined, and the sheet can't be rewritten.
    let mut nerve = specs(sample_sheet()).remove(0);
    nerve.max = 99;
    let err = add_stat(&mut f.world, &nerve, SEED, "again")
        .unwrap()
        .unwrap_err();
    assert!(err.contains("already a stat"), "{err}");
    let err = create_sheet(&mut f.world, &specs(sample_sheet()), SEED)
        .unwrap()
        .unwrap_err();
    assert!(err.contains("already written"), "{err}");
    // Director patches can't touch the values or the seed.
    for key in [Values::KEY, Dice::KEY] {
        let patch = Patch::new(vec![Op::SetProp {
            entity: f.player.into(),
            key: key.into(),
            value: json!({}),
        }]);
        assert!(f.world.apply_patch(&patch).is_err());
    }
    assert_eq!(sheet(&f.world).unwrap()["nerve"].max, 5);
    // A new stat comes only with its rules; new advancement rules can be
    // minted for an existing stat.
    let mut bare_stat = nerve.clone();
    bare_stat.name = "luck".into();
    bare_stat.advancement.clear();
    assert!(
        add_stat(&mut f.world, &bare_stat, SEED, "x")
            .unwrap()
            .is_err()
    );
    let before = stat_rules(&f.world).unwrap().len();
    let spec = AdvancementSpec {
        name: String::new(),
        when: "a night in the bell tower".into(),
        step: 2,
    };
    mint_advancement(&mut f.world, "nerve", &spec, "the tower quest")
        .unwrap()
        .unwrap();
    assert_eq!(stat_rules(&f.world).unwrap().len(), before + 1);
    assert!(
        mint_advancement(&mut f.world, "luck", &spec, "")
            .unwrap()
            .is_err()
    );
}

#[test]
fn raising_a_stat_follows_its_advancement_charter() {
    let mut f = fixture();
    let nerve_rule = advancement_of(&f.world, "nerve");
    let brawn_rule = advancement_of(&f.world, "brawn");
    let refused = |w: &mut World, by, cite| {
        adjust_stat(w, "nerve", by, "why", cite)
            .unwrap()
            .unwrap_err()
    };
    assert!(refused(&mut f.world, 1, None).contains("must cite"));
    assert!(
        refused(&mut f.world, 1, Some(brawn_rule)).contains("not an advancement rule of nerve")
    );
    assert!(refused(&mut f.world, 2, Some(nerve_rule)).contains("at most 1"));
    assert!(
        adjust_stat(&mut f.world, "nerve", -1, " ", None)
            .unwrap()
            .is_err()
    );
    assert!(
        adjust_stat(&mut f.world, "luck", -1, "x", None)
            .unwrap()
            .is_err()
    );
    assert_eq!(value(&f.world, "nerve"), 3);

    for _ in 0..4 {
        adjust_stat(&mut f.world, "nerve", 1, "faced it", Some(nerve_rule))
            .unwrap()
            .unwrap();
    }
    // Clamped at the bound.
    assert_eq!(value(&f.world, "nerve"), 5);
}

#[test]
fn checks_grade_d20_plus_stat() {
    assert_eq!(CheckResult::grade(15, 15), CheckResult::Success);
    assert_eq!(CheckResult::grade(11, 15), CheckResult::Partial);
    assert_eq!(CheckResult::grade(10, 15), CheckResult::Failure);
    for n in 0..1000 {
        assert!((1..=20).contains(&die(roll_seed(SEED, n))));
    }
}

fn rolls(world: &World, n: usize) -> Vec<(i64, CheckResult)> {
    (0..n)
        .map(|_| {
            let r = check(world, "nerve", 14, json!({})).unwrap().unwrap();
            (r.die, r.result)
        })
        .collect()
}

#[test]
fn rolls_are_deterministic_and_logged() {
    let a = fixture();
    let b = fixture();
    let seq = rolls(&a.world, 20);
    assert_eq!(seq, rolls(&b.world, 20));
    // The dice really vary.
    let faces: std::collections::BTreeSet<i64> = seq.iter().map(|r| r.0).collect();
    assert!(faces.len() > 5, "{seq:?}");
    let mut other = bare();
    create_sheet(&mut other.world, &specs(sample_sheet()), SEED + 1)
        .unwrap()
        .unwrap();
    assert_ne!(seq, rolls(&other.world, 20));

    // Each roll is logged with what it needs to be checked.
    let logged: Vec<Value> = a
        .world
        .events()
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EventKind::Roll)
        .map(|e| e.payload)
        .collect();
    assert_eq!(logged.len(), 20);
    assert_eq!(logged[3]["n"], json!(3));
    assert_eq!(logged[3]["seed"], json!(roll_seed(SEED, 3)));
    assert_eq!(logged[3]["die"], json!(seq[3].0));
    assert_eq!(logged[3]["total"], json!(seq[3].0 + 3));
    assert_eq!(logged[3]["result"], json!(seq[3].1));

    // Replay rebuilds the same world and log; a fork mid-way rolls what the
    // original rolled from there on.
    let replayed = a.world.replay_in_memory().unwrap();
    assert_eq!(replayed.state().unwrap(), a.world.state().unwrap());
    assert_eq!(replayed.events().unwrap(), a.world.events().unwrap());
    let tenth_roll = a
        .world
        .events()
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EventKind::Roll)
        .nth(9)
        .unwrap()
        .seq;
    let fork = a.world.fork_in_memory(tenth_roll).unwrap();
    assert_eq!(rolls(&fork, 10), seq[10..]);
}

#[test]
fn a_capacity_rule_drives_the_carry_limit() {
    let mut f = fixture();
    let inventory = Inventory::new(DEFAULT_SLOTS, carry_limit);
    assert_eq!(inventory.limit(&f.world, f.player).unwrap(), 10.0);
    lower(&mut f.world, "brawn", 2);
    assert_eq!(inventory.limit(&f.world, f.player).unwrap(), 6.0);
    // Other carriers, and a world without a sheet, get the default.
    assert_eq!(
        inventory.limit(&f.world, f.room).unwrap(),
        DEFAULT_CARRY_LIMIT
    );
    let b = bare();
    assert_eq!(
        carry_limit(&b.world, b.player).unwrap(),
        DEFAULT_CARRY_LIMIT
    );
}

/// A world rule embedding a nerve check, minted.
fn creaking_stair(world: &mut World, difficulty: i64) -> rules::Rule {
    let def: RuleDef = serde_json::from_value(json!({
        "name": "Creeping up the stair", "trigger": "creep up <stair>",
        "roles": {"stair": "the stair"},
        "conditions": [{"check": "kind", "role": "stair", "is": "stair"}],
        "effects": [{"op": "set_prop", "entity": "stair", "key": "climbed", "value": true}],
        "outcome": "You reach the top unheard.",
        "check": {"stat": "nerve", "difficulty": difficulty,
                  "partial": {"outcome": "You reach the top, but the stair groans.",
                              "effects": [{"op": "set_prop", "entity": "stair", "key": "climbed", "value": true},
                                          {"op": "set_prop", "entity": "room", "key": "alerted", "value": true}]},
                  "failure": {"outcome": "You freeze halfway and creep back down.",
                              "adjust": [{"stat": "nerve", "by": -1}]}}
    }))
    .unwrap();
    rules::mint(world, def, "").unwrap().unwrap().0
}

#[test]
fn a_rule_can_embed_a_check() {
    let mut f = fixture();
    let ops: Vec<Op> = serde_json::from_value(json!([
        {"op": "create", "name": "stair", "kind": "stair", "location": f.room}
    ]))
    .unwrap();
    let stair = f
        .world
        .apply_engine_patch(&Patch::new(ops))
        .unwrap()
        .created[0];
    let scope = Scope {
        player: f.player,
        room: f.room,
    };
    let bindings = rules::Bindings::from([("stair".to_owned(), stair)]);
    // Pick each difficulty from the die the next roll will show, so each
    // result comes up once: success, partial, failure.
    let next_die = |w: &World| {
        die(roll_seed(
            SEED,
            w.count_events(EventKind::Roll).unwrap() as u64,
        ))
    };
    let nerve = 3;
    let mut outcomes = Vec::new();
    for margin in [0, 2, 10] {
        let difficulty = next_die(&f.world) + nerve + margin;
        let rule = creaking_stair(&mut f.world, difficulty);
        let app = rules::apply(&mut f.world, &rule.def, rules::tag(&rule), scope, &bindings)
            .unwrap()
            .unwrap();
        outcomes.push((
            app.roll.unwrap().result,
            app.outcome,
            app.stats.changed.len(),
        ));
        rules::retire(&mut f.world, rule.id, "next")
            .unwrap()
            .unwrap();
    }
    assert_eq!(
        outcomes,
        [
            (
                CheckResult::Success,
                "You reach the top unheard.".to_owned(),
                0
            ),
            (
                CheckResult::Partial,
                "You reach the top, but the stair groans.".to_owned(),
                0
            ),
            (
                CheckResult::Failure,
                "You freeze halfway and creep back down.".to_owned(),
                1
            ),
        ]
    );
    let room = f.world.entity(f.room).unwrap().unwrap();
    assert_eq!(room.props["alerted"], json!(true));
    assert_eq!(value(&f.world, "nerve"), 2);
    // The patch event points at its roll.
    let patch = f
        .world
        .events()
        .unwrap()
        .into_iter()
        .rev()
        .find(|e| e.kind == EventKind::Patch && e.payload.get("roll").is_some())
        .unwrap();
    let roll = patch.payload["roll"].as_i64().unwrap();
    assert!(
        f.world
            .events()
            .unwrap()
            .iter()
            .any(|e| e.seq == roll && e.kind == EventKind::Roll)
    );
}

#[test]
fn world_rules_only_lower_stats_the_sheet_has() {
    let mut f = fixture();
    let base = serde_json::to_value(creaking_stair(&mut f.world, 10).def).unwrap();
    let refused = |w: &mut World, patch: Value| {
        let mut v = base.clone();
        for (k, x) in patch.as_object().unwrap() {
            v[k] = x.clone();
        }
        rules::mint(w, serde_json::from_value(v).unwrap(), "")
            .unwrap()
            .unwrap_err()
    };
    assert!(
        refused(
            &mut f.world,
            json!({"adjust": [{"stat": "nerve", "by": 1}]})
        )
        .contains("only lower")
    );
    assert!(
        refused(
            &mut f.world,
            json!({"adjust": [{"stat": "luck", "by": -1}]})
        )
        .contains("unknown stat")
    );
    assert!(
        refused(
            &mut f.world,
            json!({"check": {"stat": "luck", "difficulty": 3}})
        )
        .contains("unknown stat")
    );
    assert!(
        refused(
            &mut f.world,
            json!({"check": {"stat": "nerve", "difficulty": 3,
                             "failure": {"adjust": [{"stat": "brawn", "by": 2}]}}})
        )
        .contains("only lower")
    );
}
