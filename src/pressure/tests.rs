use serde_json::json;

use super::*;
use crate::llm::{ToolOutput, ToolUse};
use crate::world::{Op, Patch};

/// A world with a bible (so the clock runs), a room and a sluice in it.
fn world() -> (World, EntityId, EntityId) {
    let mut world = World::open_in_memory().unwrap();
    let ops: Vec<Op> = serde_json::from_value(json!([
        {"op": "create", "name": "World bible", "kind": "bible"},
        {"op": "create", "handle": "mill", "name": "Mill", "kind": "room"},
        {"op": "create", "handle": "sluice", "name": "sluice", "kind": "gate", "location": "mill"}
    ]))
    .unwrap();
    let a = world.apply_engine_patch(&Patch::new(ops)).unwrap();
    (world, a.handles["mill"], a.handles["sluice"])
}

fn call(name: &str, input: serde_json::Value) -> ToolUse {
    ToolUse {
        id: "c".into(),
        name: name.into(),
        input,
    }
}

fn run(world: &mut World, name: &str, input: serde_json::Value) -> Result<String, String> {
    match handle_tool(world, &call(name, input)).unwrap().unwrap() {
        ToolOutput::Ok(s) => Ok(s),
        ToolOutput::Err(s) => Err(s),
    }
}

#[test]
fn urgency_boundaries() {
    // A 6-hour pressure started at 0.
    let u = |now| urgency(0, 360, now);
    assert_eq!(u(0), Urgency::Low);
    assert_eq!(u(179), Urgency::Low); // 181 left > 180
    assert_eq!(u(180), Urgency::Rising); // exactly half left
    assert_eq!(u(299), Urgency::Rising); // 61 left
    assert_eq!(u(300), Urgency::Imminent); // 60 left
    assert_eq!(u(359), Urgency::Imminent);
    assert_eq!(u(360), Urgency::Overdue);
    assert_eq!(u(1000), Urgency::Overdue);
    // A short pressure is imminent from the start.
    assert_eq!(urgency(0, 45, 0), Urgency::Imminent);
}

#[test]
fn start_adjust_and_resolve() {
    let (mut world, mill, sluice) = world();
    let out = run(
        &mut world,
        "start_pressure",
        json!({"what": "The sluice gives way", "consequence": "the lower mill floods",
               "due_in_minutes": 360, "concerns": [sluice, mill, sluice]}),
    )
    .unwrap();
    let id = active(&world).unwrap()[0].id;
    assert!(out.contains("\"urgency\":\"low\""), "{out}");
    let p = pressure(&world, id).unwrap().unwrap();
    assert_eq!(p.what, "The sluice gives way");
    assert_eq!((p.started.0, p.due.0), (480, 840));
    assert_eq!(p.concerns, vec![mill, sluice]);
    assert_eq!(world.entity(id).unwrap().unwrap().kind, PRESSURE_KIND);

    // Shoring up buys two hours; then the rain hastens it by an hour.
    run(
        &mut world,
        "adjust_pressure",
        json!({"id": id, "minutes": 120, "reason": "shored up"}),
    )
    .unwrap();
    crate::time::advance(&mut world, 30, crate::time::Source::Default).unwrap();
    run(
        &mut world,
        "adjust_pressure",
        json!({"id": id, "minutes": -60, "reason": "heavy rain"}),
    )
    .unwrap();
    let p = pressure(&world, id).unwrap().unwrap();
    assert_eq!(p.due.0, 900);
    assert_eq!(
        p.adjustments,
        vec![
            Adjustment {
                at: 480,
                minutes: 120,
                reason: "shored up".into()
            },
            Adjustment {
                at: 510,
                minutes: -60,
                reason: "heavy rain".into()
            },
        ]
    );
    // Hastening stops at now.
    run(
        &mut world,
        "adjust_pressure",
        json!({"id": id, "minutes": -10000, "reason": "collapse"}),
    )
    .unwrap();
    let p = pressure(&world, id).unwrap().unwrap();
    assert_eq!(p.due.0, 510);
    assert_eq!(p.adjustments.last().unwrap().minutes, -390);
    assert_eq!(
        p.urgency(crate::time::now(&world).unwrap()),
        Urgency::Overdue
    );

    run(
        &mut world,
        "resolve_pressure",
        json!({"id": id, "how": "the sluice burst; the mill flooded"}),
    )
    .unwrap();
    let p = pressure(&world, id).unwrap().unwrap();
    assert_eq!(p.status, Status::Resolved);
    assert_eq!(
        p.resolution.as_deref(),
        Some("the sluice burst; the mill flooded")
    );
    assert!(active(&world).unwrap().is_empty());
    // Resolved pressures can't be touched again.
    let err = run(
        &mut world,
        "adjust_pressure",
        json!({"id": id, "minutes": 5, "reason": "x"}),
    )
    .unwrap_err();
    assert!(err.contains("already resolved"), "{err}");
    let err = run(
        &mut world,
        "resolve_pressure",
        json!({"id": id, "how": "x"}),
    )
    .unwrap_err();
    assert!(err.contains("already resolved"), "{err}");

    // Replay reproduces it all.
    let replayed = world.replay_in_memory().unwrap();
    assert_eq!(replayed.state().unwrap(), world.state().unwrap());
}

#[test]
fn bad_calls_are_refused() {
    let (mut world, mill, _) = world();
    let bible = world.entities_of_kind("bible").unwrap()[0];
    let start = |what: &str, due: u64, concerns: Vec<EntityId>| json!({"what": what, "consequence": "c", "due_in_minutes": due, "concerns": concerns});
    for (input, want) in [
        (start(" ", 60, vec![]), "what must not be empty"),
        (start("x", 0, vec![]), "due_in_minutes must be"),
        (start("x", MAX_DUE_IN + 1, vec![]), "due_in_minutes must be"),
        (start("x", 60, vec![999]), "does not exist"),
        (start("x", 60, vec![bible]), "is a bible"),
    ] {
        let err = run(&mut world, "start_pressure", input).unwrap_err();
        assert!(err.contains(want), "{err}");
    }
    let err = run(
        &mut world,
        "adjust_pressure",
        json!({"id": mill, "minutes": 5, "reason": "r"}),
    )
    .unwrap_err();
    assert!(err.contains("is not a pressure"), "{err}");
    run(&mut world, "start_pressure", start("x", 60, vec![])).unwrap();
    let id = active(&world).unwrap()[0].id;
    let err = run(
        &mut world,
        "adjust_pressure",
        json!({"id": id, "minutes": 0, "reason": "r"}),
    )
    .unwrap_err();
    assert!(err.contains("non-zero"), "{err}");
    // A pressure can't concern another pressure.
    let err = run(&mut world, "start_pressure", start("y", 60, vec![id])).unwrap_err();
    assert!(err.contains("is a pressure"), "{err}");
    // The director can't create one by patch.
    let patch: Patch =
        serde_json::from_value(json!({"ops": [{"op": "create", "name": "x", "kind": "pressure"}]}))
            .unwrap();
    assert!(world.apply_patch(&patch).is_err());
}

#[test]
fn slice_lists_active_pressures_nearest_first_with_a_nudge_when_overdue() {
    let (mut world, _, sluice) = world();
    let st = |world: &mut World, what: &str, due: u64, concerns: Vec<EntityId>| {
        start(world, what, "it happens", due, &concerns).unwrap()
    };
    let far = st(&mut world, "The fair ends", 600, vec![]);
    let near = st(&mut world, "The sluice gives way", 90, vec![sluice]);
    let done = st(&mut world, "The bell rings", 30, vec![]);
    resolve(&mut world, done, "rung").unwrap();
    assert!(
        slice_lines(&world, Audience::Director, |_| Ok(None))
            .unwrap()
            .len()
            == 2
    );
    crate::time::advance(&mut world, 120, crate::time::Source::Default).unwrap();
    let lines = slice_lines(&world, Audience::Director, |p| {
        Ok((p.id == near).then(|| "marked".to_owned()))
    })
    .unwrap();
    assert_eq!(
        lines,
        vec![
            format!(
                "- pressure #{near} The sluice gives way [overdue] due day 1, 09:30 \
                 (morning) (30 min ago); then: it happens (concerns #{sluice}) (marked). \
                 {OVERDUE_NUDGE}"
            ),
            format!(
                "- pressure #{far} The fair ends [low] due day 1, 18:00 (dusk) (in 8 h); \
                 then: it happens"
            ),
        ]
    );
}

#[test]
fn an_overdue_pressure_on_a_quest_nudges_the_director_to_escalate_it() {
    let (mut world, _, sluice) = world();
    let open = crate::quest::spawn(&mut world, "Save the mill", "water rises", &[sluice]).unwrap();
    let done = crate::quest::spawn(&mut world, "Find the miller", "gone", &[sluice]).unwrap();
    // The judge alone resolves quests; write the state as it would.
    let mut state = world.entity(done).unwrap().unwrap().props["_quest"].clone();
    state["status"] = json!("complete");
    let op = Op::SetProp {
        entity: done.into(),
        key: "_quest".into(),
        value: state,
    };
    world.apply_engine_patch(&Patch::new(vec![op])).unwrap();
    let p = start(
        &mut world,
        "The flood",
        "the mill drowns",
        60,
        &[sluice, open, done],
    )
    .unwrap();

    let nudge = quest_nudge(&crate::quest::quest(&world, open).unwrap().unwrap());
    assert!(
        nudge.contains(&format!("escalate_quest #{open}")),
        "{nudge}"
    );
    // Not before it is due.
    let lines = slice_lines(&world, Audience::Director, |_| Ok(None)).unwrap();
    assert!(!lines[0].contains("escalate_quest"), "{}", lines[0]);

    crate::time::advance(&mut world, 60, crate::time::Source::Default).unwrap();
    let lines = slice_lines(&world, Audience::Director, |_| Ok(None)).unwrap();
    assert_eq!(
        lines,
        vec![format!(
            "- pressure #{p} The flood [overdue] due day 1, 09:00 (morning) (now); \
             then: the mill drowns (concerns #{sluice} #{open} #{done}). {OVERDUE_NUDGE}. {nudge}"
        )]
    );
    // The tick has no quest tools: it gets the plain nudge.
    let lines = slice_lines(&world, Audience::Tick, |_| Ok(None)).unwrap();
    assert!(lines[0].ends_with(OVERDUE_NUDGE), "{}", lines[0]);
    // Missing the deadline fails nothing.
    let q = crate::quest::quest(&world, open).unwrap().unwrap();
    assert_eq!(q.status, crate::quest::Status::Open);
}
