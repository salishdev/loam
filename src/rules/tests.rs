use serde_json::{Value, json};

use super::*;
use crate::world::CARRIER_PROP;

/// A room holding a rotten door, a sound door and a crate with a nail in
/// it; a player in the room carrying a crowbar; a far room with a gate.
struct Fixture {
    world: World,
    scope: Scope,
    rotten: EntityId,
    sound: EntityId,
    nail: EntityId,
    crowbar: EntityId,
    gate: EntityId,
}

fn fixture() -> Fixture {
    let mut world = World::open_in_memory().unwrap();
    let ops: Vec<Op> = serde_json::from_value(json!([
        {"op": "create", "handle": "hall", "name": "Hall", "kind": "room"},
        {"op": "create", "handle": "far", "name": "Far room", "kind": "room"},
        {"op": "create", "handle": "you", "name": "you", "kind": "player", "location": "hall",
         "props": {CARRIER_PROP: true}},
        {"op": "create", "handle": "rotten", "name": "oak door", "kind": "door", "location": "hall",
         "props": {"rotten": true}},
        {"op": "create", "handle": "sound", "name": "iron door", "kind": "door", "location": "hall"},
        {"op": "create", "handle": "crate", "name": "crate", "kind": "container", "location": "hall"},
        {"op": "create", "handle": "nail", "name": "nail", "kind": "item", "location": "crate"},
        {"op": "create", "handle": "bar", "name": "crowbar", "kind": "item", "location": "you"},
        {"op": "create", "handle": "gate", "name": "gate", "kind": "door", "location": "far"}
    ]))
    .unwrap();
    let a = world.apply_engine_patch(&Patch::new(ops)).unwrap();
    let h = |n: &str| a.handles[n];
    Fixture {
        scope: Scope {
            player: h("you"),
            room: h("hall"),
        },
        rotten: h("rotten"),
        sound: h("sound"),
        nail: h("nail"),
        crowbar: h("bar"),
        gate: h("gate"),
        world,
    }
}

fn kick_rule() -> RuleDef {
    serde_json::from_value(json!({
        "name": "Kicking a rotten door",
        "trigger": "kick or shoulder <target>",
        "roles": {"target": "the door kicked"},
        "conditions": [
            {"check": "kind", "role": "target", "is": "door"},
            {"check": "prop", "role": "target", "prop": "rotten", "is": true},
            {"check": "not_prop", "role": "target", "prop": "broken", "is": true}
        ],
        "effects": [
            {"op": "set_prop", "entity": "target", "key": "broken", "value": true},
            {"op": "create", "handle": "bits", "name": "splinters", "kind": "debris", "location": "room",
             "props": {"state": "damp"}}
        ],
        "outcome": "The door gives way in wet splinters."
    }))
    .unwrap()
}

fn bind_target(id: EntityId) -> Bindings {
    Bindings::from([("target".to_owned(), id)])
}

#[test]
fn a_general_rule_applies_to_every_matching_entity_the_same_way() {
    let mut f = fixture();
    let (rule, _) = mint(&mut f.world, kick_rule(), RuleClass::World, "")
        .unwrap()
        .unwrap();
    let app = apply(
        &mut f.world,
        &rule.def,
        tag(&rule),
        f.scope,
        &bind_target(f.rotten),
    )
    .unwrap()
    .unwrap();
    let door = f.world.entity(f.rotten).unwrap().unwrap();
    assert_eq!(door.props["broken"], json!(true));
    let splinters = f.world.entity(app.applied.created[0]).unwrap().unwrap();
    assert_eq!(splinters.name, "splinters");
    assert_eq!(splinters.location, Some(f.scope.room));
    // The patch event names the rule and the bindings.
    let event = f
        .world
        .events()
        .unwrap()
        .into_iter()
        .rev()
        .find(|e| e.kind == EventKind::Patch && e.payload.get("rule").is_some())
        .unwrap();
    assert_eq!(event.payload["rule"], json!({"id": rule.id, "version": 1}));
    assert_eq!(event.payload["bindings"]["target"], json!(f.rotten));

    // Kicking it again: the door is already broken.
    let again = apply(
        &mut f.world,
        &rule.def,
        tag(&rule),
        f.scope,
        &bind_target(f.rotten),
    )
    .unwrap();
    assert!(
        matches!(&again, Err(Refusal::Unmet(r)) if r == &["target.broken should not be true, is true"]),
        "{again:?}"
    );
    // A sound door doesn't match.
    let sound = apply(
        &mut f.world,
        &rule.def,
        tag(&rule),
        f.scope,
        &bind_target(f.sound),
    )
    .unwrap();
    assert!(
        matches!(&sound, Err(Refusal::Unmet(r)) if r == &["target.rotten should be true, is absent"]),
        "{sound:?}"
    );
}

#[test]
fn bindings_must_be_in_scope_and_complete() {
    let f = fixture();
    let rule = kick_rule();
    let bound = |given: Bindings| bind(&f.world, &rule, f.scope, &given).unwrap();
    assert!(bound(bind_target(f.gate)).unwrap_err().contains("not here"));
    assert!(bound(Bindings::new()).unwrap_err().contains("not bound"));
    assert!(
        bound(Bindings::from([("victim".to_owned(), f.rotten)]))
            .unwrap_err()
            .contains("no role")
    );
    // Things inside containers and carried things are in scope; built-ins
    // are filled in and can't be overridden.
    let b = bound(Bindings::from([
        ("target".to_owned(), f.nail),
        ("room".to_owned(), f.gate),
    ]))
    .unwrap();
    assert_eq!(b["room"], f.scope.room);
    assert_eq!(b["player"], f.scope.player);
    assert!(bound(bind_target(f.crowbar)).is_ok());
}

#[test]
fn carried_and_entity_conditions() {
    let f = fixture();
    let rule: RuleDef = serde_json::from_value(json!({
        "name": "Prise", "trigger": "prise <target> with <tool>",
        "roles": {"target": "", "tool": ""},
        "conditions": [{"check": "carried", "role": "tool"}, {"check": "entity", "role": "target", "id": f.rotten}]
    }))
    .unwrap();
    let b = |target, tool| {
        let b = bind(
            &f.world,
            &rule,
            f.scope,
            &Bindings::from([("target".to_owned(), target), ("tool".to_owned(), tool)]),
        )
        .unwrap()
        .unwrap();
        unmet(&f.world, &rule, f.scope, &b).unwrap()
    };
    assert!(b(f.rotten, f.crowbar).is_empty());
    assert_eq!(
        b(f.sound, f.nail),
        vec![
            "tool is not carried".to_owned(),
            format!("target is #{}, not #{}", f.sound, f.rotten)
        ]
    );
}

#[test]
fn effects_can_reach_fixed_entities_elsewhere() {
    let mut f = fixture();
    let mut rule = kick_rule();
    rule.effects = vec![Op::SetProp {
        entity: f.gate.into(),
        key: "open".into(),
        value: Value::Bool(true),
    }];
    let app = apply(
        &mut f.world,
        &rule,
        json!(null),
        f.scope,
        &bind_target(f.rotten),
    )
    .unwrap()
    .unwrap();
    assert_eq!(app.new_terms, vec!["open"]);
    assert_eq!(
        f.world.entity(f.gate).unwrap().unwrap().props["open"],
        json!(true)
    );
}

#[test]
fn rejected_effects_change_nothing() {
    let mut f = fixture();
    let mut rule = kick_rule();
    rule.effects = serde_json::from_value(json!([
        {"op": "set_prop", "entity": "target", "key": "broken", "value": true},
        {"op": "move", "entity": "target", "to": "player"}
    ]))
    .unwrap();
    let r = apply(
        &mut f.world,
        &rule,
        json!(null),
        f.scope,
        &bind_target(f.rotten),
    )
    .unwrap();
    assert!(
        matches!(&r, Err(Refusal::Rejected(m)) if m.contains("inventory")),
        "{r:?}"
    );
    assert!(
        !f.world
            .entity(f.rotten)
            .unwrap()
            .unwrap()
            .props
            .contains_key("broken")
    );
}

#[test]
fn invalid_definitions_are_refused() {
    let base = kick_rule();
    let refused = |patch: Value| {
        let mut v = serde_json::to_value(&base).unwrap();
        for (k, x) in patch.as_object().unwrap() {
            v[k] = x.clone();
        }
        validate(&serde_json::from_value(v).unwrap()).unwrap_err()
    };
    assert!(refused(json!({"trigger": " "})).contains("trigger"));
    assert!(refused(json!({"roles": {"room": "x"}})).contains("built in"));
    assert!(
        refused(json!({"conditions": [{"check": "kind", "role": "ghost", "is": "x"}]}))
            .contains("undeclared role")
    );
    assert!(
        refused(json!({"effects": [{"op": "destroy", "entity": "ghost"}]}))
            .contains("neither a role")
    );
    assert!(
        refused(
            json!({"effects": [{"op": "create", "handle": "target", "name": "x", "kind": "x"}]})
        )
        .contains("clashes")
    );
    assert!(
        refused(json!({"effects": [{"op": "set_prop", "entity": "target", "key": "_carrier", "value": true}]}))
            .contains("engine-owned")
    );
    assert!(validate(&base).is_ok());
}

#[test]
fn amendments_and_retirement_are_versioned_in_the_log() {
    let mut f = fixture();
    let (rule, terms) = mint(&mut f.world, kick_rule(), RuleClass::World, "")
        .unwrap()
        .unwrap();
    assert!(
        terms.contains(&"rotten".to_owned()) && terms.contains(&"broken".to_owned()),
        "{terms:?}"
    );
    let amended = amend(
        &mut f.world,
        rule.id,
        Amendment {
            outcome: Some("It bursts.".into()),
            ..Amendment::default()
        },
        "too wordy",
    )
    .unwrap()
    .unwrap()
    .0;
    assert_eq!(amended.version, 2);
    assert_eq!(amended.def.outcome, "It bursts.");
    assert_eq!(amended.def.conditions, rule.def.conditions);
    // An amendment that breaks the rule is refused and changes nothing.
    let bad = amend(
        &mut f.world,
        rule.id,
        Amendment {
            roles: Some(BTreeMap::new()),
            ..Amendment::default()
        },
        "",
    )
    .unwrap();
    assert!(bad.is_err());
    assert_eq!(get(&f.world, rule.id).unwrap().unwrap(), amended);

    let retired = retire(&mut f.world, rule.id, "superseded")
        .unwrap()
        .unwrap();
    assert_eq!((retired.version, retired.status), (3, RuleStatus::Retired));
    assert!(active(&f.world).unwrap().is_empty());
    assert_eq!(all(&f.world).unwrap().len(), 1);
    assert!(
        retire(&mut f.world, rule.id, "")
            .unwrap()
            .unwrap_err()
            .contains("retired")
    );

    let changes: Vec<(String, i64)> = f
        .world
        .events()
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EventKind::RuleChange)
        .map(|e| {
            (
                e.payload["action"].as_str().unwrap().to_owned(),
                e.payload["version"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        changes,
        [
            ("minted".to_owned(), 1),
            ("amended".to_owned(), 2),
            ("retired".to_owned(), 3)
        ]
    );

    // Rules live in the world store, so replay rebuilds them.
    let replayed = f.world.replay_in_memory().unwrap();
    assert_eq!(all(&replayed).unwrap(), all(&f.world).unwrap());
}

#[test]
fn stat_rules_never_change() {
    let mut f = fixture();
    let (rule, _) = mint(&mut f.world, kick_rule(), RuleClass::Stat, "")
        .unwrap()
        .unwrap();
    let err = amend(&mut f.world, rule.id, Amendment::default(), "")
        .unwrap()
        .unwrap_err();
    assert!(err.contains("stat rule"), "{err}");
    let err = retire(&mut f.world, rule.id, "").unwrap().unwrap_err();
    assert!(err.contains("stat rule"), "{err}");
    assert_eq!(get(&f.world, rule.id).unwrap().unwrap(), rule);
}
