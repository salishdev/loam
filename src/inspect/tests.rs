use serde_json::json;

use super::*;
use crate::glossary::Definition;
use crate::rules::{Bindings, RuleDef, Scope};
use crate::world::{CARRIER_PROP, Patch};

/// A hall with a player, an oak door (rotten), an iron door and a crowbar.
struct Fixture {
    world: World,
    scope: Scope,
    oak: EntityId,
    iron: EntityId,
    crowbar: EntityId,
}

fn fixture() -> Fixture {
    let mut world = World::open_in_memory().unwrap();
    world
        .log_event(
            EventKind::Input,
            &json!({"text": "a damp keep", "seed": true}),
        )
        .unwrap();
    let ops: Vec<Op> = serde_json::from_value(json!([
        {"op": "create", "handle": "hall", "name": "Hall", "kind": "room"},
        {"op": "create", "handle": "you", "name": "you", "kind": "player", "location": "hall",
         "props": {CARRIER_PROP: true}},
        {"op": "create", "handle": "oak", "name": "oak door", "kind": "door", "location": "hall",
         "props": {"rotten": true}},
        {"op": "create", "handle": "iron", "name": "iron door", "kind": "door", "location": "hall"},
        {"op": "create", "handle": "bar", "name": "crowbar", "kind": "item", "location": "hall"}
    ]))
    .unwrap();
    let a = world.apply_engine_patch(&Patch::new(ops)).unwrap();
    let h = |n: &str| a.handles[n];
    Fixture {
        scope: Scope {
            player: h("you"),
            room: h("hall"),
        },
        oak: h("oak"),
        iron: h("iron"),
        crowbar: h("bar"),
        world,
    }
}

fn kick_rule() -> RuleDef {
    serde_json::from_value(json!({
        "name": "Kicking a rotten door",
        "trigger": "kick <target>",
        "roles": {"target": "the door kicked"},
        "conditions": [
            {"check": "kind", "role": "target", "is": "door"},
            {"check": "prop", "role": "target", "prop": "rotten", "is": true}
        ],
        "effects": [
            {"op": "set_prop", "entity": "target", "key": "broken", "value": true}
        ],
        "outcome": "The door gives way."
    }))
    .unwrap()
}

/// Mint the kick rule during a turn ("kick the oak door") and apply it.
fn kick_turn(f: &mut Fixture) -> rules::Rule {
    f.world
        .log_event(EventKind::Input, &json!({"text": "kick the oak door"}))
        .unwrap();
    let (rule, _) = rules::mint(&mut f.world, kick_rule(), "rotten wood")
        .unwrap()
        .unwrap();
    rules::apply(
        &mut f.world,
        &rule.def,
        rules::tag(&rule),
        f.scope,
        &Bindings::from([("target".to_owned(), f.oak)]),
    )
    .unwrap()
    .unwrap();
    f.world
        .log_event(EventKind::Narration, &json!({"text": "It splinters."}))
        .unwrap();
    rule
}

#[test]
fn rules_lists_every_rule_with_its_parts_and_marks_retired_ones() {
    let mut f = fixture();
    assert_eq!(rules(&f.world).unwrap(), "No rules yet.");
    let rule = kick_turn(&mut f);
    rules::retire(&mut f.world, rule.id, "too broad")
        .unwrap()
        .unwrap();
    let out = rules(&f.world).unwrap();
    assert_eq!(
        out,
        format!(
            "#{} Kicking a rotten door [world v2] RETIRED\n  \
             when: kick <target>\n  \
             role target: the door kicked\n  \
             if target is a door\n  \
             if target.rotten = true\n  \
             do set 'target'.broken = true\n  \
             => The door gives way.",
            rule.id
        )
    );
}

#[test]
fn glossary_shows_meanings_values_and_undefined_terms() {
    let mut f = fixture();
    kick_turn(&mut f);
    glossary::define(
        &mut f.world,
        &[Definition {
            name: "rotten".into(),
            meaning: "true: wood soft enough to break".into(),
            values: BTreeMap::from([("true".into(), "soft".into())]),
            free_text: None,
        }],
    )
    .unwrap()
    .unwrap();
    let out = glossary(&f.world).unwrap();
    assert!(out.contains("rotten: true: wood soft enough to break\n  = true: soft"));
    assert!(out.contains("broken: (undefined)"), "{out}");
    assert!(out.contains("description [free text]: lasting physical description"));
}

#[test]
fn quests_lists_each_quest_with_its_state() {
    let mut f = fixture();
    assert_eq!(quests(&f.world).unwrap(), "No quests yet.");
    let q = quest::spawn(&mut f.world, "The way out", "The doors are shut.", &[f.oak]).unwrap();
    assert_eq!(
        quests(&f.world).unwrap(),
        format!("#{q} The way out [open]\n  The doors are shut.")
    );
}

#[test]
fn why_shows_state_and_the_trail_grouped_by_input() {
    let mut f = fixture();
    let rule = kick_turn(&mut f);
    let out = why(&f.world, &format!("#{}", f.oak)).unwrap();
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines[0],
        format!("#{} oak door (door) in #{} Hall", f.oak, f.scope.room)
    );
    assert!(lines.contains(&"  broken = true"), "{out}");
    assert!(lines.contains(&"[seed, #1]"), "{out}");
    assert!(
        lines.iter().any(|l| l.starts_with(&format!(
            "  #2 patch by engine: create #{} \"oak door\"",
            f.oak
        ))),
        "{out}"
    );
    let header = lines
        .iter()
        .position(|l| l.starts_with("> kick the oak door"))
        .expect(&out);
    assert!(
        lines[header + 1].ends_with(&format!(
            "patch by rule #{} v1 player=#{} room=#{} target=#{}: set #{}.broken = true",
            rule.id, f.scope.player, f.scope.room, f.oak, f.oak
        )),
        "{out}"
    );
    // The iron door wasn't kicked: its trail is only its creation.
    let iron = why(&f.world, "iron door").unwrap();
    assert!(!iron.contains("kick"), "{iron}");
}

#[test]
fn why_on_a_rule_shows_its_changes_and_applications() {
    let mut f = fixture();
    let rule = kick_turn(&mut f);
    let out = why(&f.world, &rule.id.to_string()).unwrap();
    assert!(out.contains("rule minted v1: rotten wood"), "{out}");
    assert!(
        out.contains(&format!("patch by rule #{} v1", rule.id)),
        "{out}"
    );
}

#[test]
fn why_finds_entities_by_name_and_explains_misses() {
    let f = fixture();
    assert!(
        why(&f.world, "CROWBAR")
            .unwrap()
            .starts_with(&format!("#{} crowbar", f.crowbar))
    );
    let both = why(&f.world, "door").unwrap();
    assert!(
        both.starts_with("\"door\" matches several entities"),
        "{both}"
    );
    assert!(both.contains(&format!("#{} oak door (door)", f.oak)));
    assert!(both.contains(&format!("#{} iron door (door)", f.iron)));
    assert_eq!(
        why(&f.world, "dragon").unwrap(),
        "No entity matches \"dragon\"."
    );
    assert_eq!(why(&f.world, " ").unwrap(), "Usage: :why <#id | name>");
}

#[test]
fn why_follows_destroyed_entities_and_rejections_through_the_log() {
    let mut f = fixture();
    let destroy = Patch::new(vec![Op::Destroy {
        entity: f.crowbar.into(),
    }]);
    let bad = Patch::new(vec![
        Op::SetProp {
            entity: f.crowbar.into(),
            key: "bent".into(),
            value: json!(true),
        },
        Op::Destroy {
            entity: f.scope.room.into(),
        },
    ]);
    assert!(f.world.apply_patch(&bad).is_err());
    f.world.apply_patch(&destroy).unwrap();
    let out = why(&f.world, &f.crowbar.to_string()).unwrap();
    assert!(
        out.starts_with(&format!("#{} no longer exists", f.crowbar)),
        "{out}"
    );
    assert!(out.contains("REJECTED patch by director: set #"), "{out}");
    assert!(out.contains("(op 1: cannot destroy"), "{out}");
    assert!(
        out.contains(&format!("patch by director: destroy #{}", f.crowbar)),
        "{out}"
    );
}

#[test]
fn why_includes_ticks_and_judgments_that_name_the_entity() {
    let f = fixture();
    let room = f.scope.room;
    f.world
        .log_event(
            EventKind::Tick,
            &json!({"turn": 1, "room": 99, "summary": "Rain floods the hall.",
                    "changes": [{"note": "flood", "rooms": [room]}]}),
        )
        .unwrap();
    f.world
        .log_event(
            EventKind::Judgment,
            &json!({"quest": 50, "verdict": "ongoing", "reason": "not yet", "touched": [f.oak]}),
        )
        .unwrap();
    let hall = why(&f.world, "Hall").unwrap();
    assert!(
        hall.contains("[before play]") || hall.contains("[seed"),
        "{hall}"
    );
    assert!(hall.contains("world tick: Rain floods the hall."), "{hall}");
    assert!(!hall.contains("judged"), "{hall}");
    let oak = why(&f.world, "oak door").unwrap();
    assert!(oak.contains("quest #50 judged ongoing: not yet"), "{oak}");
}

#[test]
fn stats_counts_events_turns_rules_and_entities() {
    let mut f = fixture();
    let rule = kick_turn(&mut f);
    f.world
        .log_event(
            EventKind::Judgment,
            &json!({"by": "interpreter", "verdict": "fast_path", "rule": rules::tag(&rule)}),
        )
        .unwrap();
    f.world
        .log_event(
            EventKind::ToolCall,
            &json!({"name": "x", "input": {}, "result": "no", "is_error": true}),
        )
        .unwrap();
    let out = stats(&f.world).unwrap();
    for want in [
        "  input: 2",
        "Turns: 1 (1 fast path, 0 escalated by the interpreter)",
        "Tool calls: 1 (1 errors)",
        "Rules: 1 (1 world active, 0 world retired, 0 stat)",
        "Quests: 0",
        "Entities: 7 (door 2, glossary 1, item 1, player 1, room 1, rule 1)",
    ] {
        assert!(out.contains(want), "missing {want:?} in\n{out}");
    }
}
