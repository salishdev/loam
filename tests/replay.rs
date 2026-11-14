//! Replay and branching: record a session driven by a scripted director,
//! rebuild it from the event log alone, and fork it.

use loam::llm::{
    Message, Model, Request, Response, ScriptedClient, ToolOutput, ToolUse, run_tool_loop,
};
use loam::world::{ApplyError, EventKind, Op, Patch, ReplayError, World};
use serde_json::{Value, json};

/// A director tool handler that applies patches and logs each call.
fn handle(world: &mut World, call: &ToolUse) -> ToolOutput {
    assert_eq!(call.name, "apply_patch");
    let patch: Patch = serde_json::from_value(call.input.clone()).unwrap();
    let out = match world.apply_patch(&patch) {
        Ok(a) => ToolOutput::Ok(json!({ "handles": a.handles }).to_string()),
        Err(ApplyError::Rejected(r)) => ToolOutput::Err(r.to_string()),
        Err(e) => panic!("storage error: {e}"),
    };
    let result = match &out {
        ToolOutput::Ok(s) | ToolOutput::Err(s) => s.clone(),
    };
    world
        .log_event(
            EventKind::ToolCall,
            &json!({ "name": call.name, "input": call.input, "result": result }),
        )
        .unwrap();
    out
}

/// One player turn: log the input, then let the scripted director run.
fn turn(world: &mut World, input: &str, director: Vec<Response>) {
    world
        .log_event(EventKind::Input, &json!({ "text": input }))
        .unwrap();
    let client = ScriptedClient::new(director);
    let request = Request::new(Model::Sonnet, vec![Message::user(input)]);
    run_tool_loop(&client, request, 10, &mut |_| {}, &mut |c| handle(world, c)).unwrap();
    assert_eq!(client.remaining(), 0);
}

fn patch_call(id: &str, ops: Value) -> Response {
    Response::canned_tool_use(id, "apply_patch", json!({ "ops": ops }))
}

/// Record a short session: world creation, a rejected patch, a dice roll
/// and judgment, moves, prop changes, relations and a destroy that
/// cascades one.
fn record_session() -> World {
    let mut w = World::open_in_memory().unwrap();
    turn(
        &mut w,
        "a drowned abbey at dusk",
        vec![
            patch_call(
                "t1",
                json!([
                    {"op": "create", "handle": "nave", "name": "Nave", "kind": "room"},
                    {"op": "create", "handle": "crypt", "name": "Crypt", "kind": "room"},
                    {"op": "create", "handle": "door", "name": "Rotten door", "kind": "door",
                     "location": "nave", "props": {"rotten": true}},
                    {"op": "create", "handle": "me", "name": "You", "kind": "player",
                     "location": "nave", "props": {"hp": 10}},
                    {"op": "create", "handle": "lamp", "name": "Lamp", "kind": "item",
                     "location": "me"},
                ]),
            ),
            Response::canned_text("Water laps at the pews."),
        ],
    );
    turn(
        &mut w,
        "put the nave in my lamp",
        vec![
            // Containment cycle: rejected, logged, and must be skipped on replay.
            patch_call("t2", json!([{"op": "move", "entity": 4, "to": 5}])),
            Response::canned_text("That makes no sense."),
        ],
    );
    turn(
        &mut w,
        "kick the rotten door",
        vec![
            patch_call(
                "t3",
                json!([
                    {"op": "set_prop", "entity": 3, "key": "broken", "value": true},
                    {"op": "remove_prop", "entity": 3, "key": "rotten"},
                    {"op": "create", "handle": "sp", "name": "Splinters", "kind": "scenery",
                     "location": 1},
                    {"op": "relate", "subject": "sp", "rel": "broken_from", "object": 3,
                     "props": {"pieces": 12}},
                    {"op": "relate", "subject": 3, "rel": "guards", "object": 2},
                ]),
            ),
            Response::canned_text("The door bursts."),
        ],
    );
    w.log_event(
        EventKind::Roll,
        &json!({"stat": "might", "difficulty": 12, "seed": 42, "roll": 7, "result": "failure"}),
    )
    .unwrap();
    turn(
        &mut w,
        "climb down into the crypt",
        vec![
            patch_call(
                "t4",
                json!([
                    {"op": "move", "entity": 4, "to": 2},
                    {"op": "set_prop", "entity": 4, "key": "hp", "value": 8},
                    {"op": "destroy", "entity": 6},
                    {"op": "rename", "entity": 2, "name": "Flooded crypt"},
                    {"op": "unrelate", "subject": 3, "rel": "guards", "object": 2},
                    {"op": "relate", "subject": 4, "rel": "fears", "object": 2},
                ]),
            ),
            Response::canned_text("You slip and fall."),
        ],
    );
    w.log_event(
        EventKind::Judgment,
        &json!({"quest": "reach the crypt", "complete": true}),
    )
    .unwrap();
    w
}

#[test]
fn replay_rebuilds_identical_state_and_log() {
    let original = record_session();
    let kinds: Vec<_> = original.events().unwrap().iter().map(|e| e.kind).collect();
    for k in [
        EventKind::Input,
        EventKind::Patch,
        EventKind::PatchRejected,
        EventKind::ToolCall,
        EventKind::Roll,
        EventKind::Judgment,
    ] {
        assert!(kinds.contains(&k), "session should record {k:?}");
    }

    // No client exists here: replay cannot call an LLM.
    let replayed = original.replay_in_memory().unwrap();

    assert_eq!(replayed.state().unwrap(), original.state().unwrap());
    assert_eq!(replayed.events().unwrap(), original.events().unwrap());
    assert_eq!(replayed.forked_at().unwrap(), None);
    let crypt = replayed.entity(2).unwrap().unwrap();
    assert_eq!(crypt.name, "Flooded crypt");
    assert_eq!(replayed.contents(Some(2)).unwrap(), vec![4]);
    assert!(replayed.entity(6).unwrap().is_none());
    let relations: Vec<_> = replayed
        .state()
        .unwrap()
        .relations
        .iter()
        .map(|r| (r.subject, r.rel.clone(), r.object))
        .collect();
    assert_eq!(relations, [(4, "fears".to_owned(), 2)]);
}

#[test]
fn relations_and_cascades_replay_and_fork() {
    let original = record_session();
    let events = original.events().unwrap();
    let patches: Vec<_> = events
        .iter()
        .filter(|e| e.kind == EventKind::Patch)
        .collect();
    let (kick, climb) = (patches[1], patches[2]);
    assert!(kick.payload.get("cascaded").is_none());
    assert_eq!(
        climb.payload["cascaded"],
        json!([{"op": 2, "subject": 6, "rel": "broken_from", "object": 3,
                "props": {"pieces": 12}}])
    );

    // Right after the kick, both relations hold.
    let state = state_at(kick.seq);
    let relations: Vec<_> = state
        .relations
        .iter()
        .map(|r| {
            (
                r.subject,
                r.rel.as_str(),
                r.object,
                Value::Object(r.props.clone()),
            )
        })
        .collect();
    assert_eq!(
        relations,
        [
            (3, "guards", 2, json!({})),
            (6, "broken_from", 3, json!({"pieces": 12})),
        ]
    );
    // Forking after the cascade matches the session's end exactly.
    assert_eq!(state_at(climb.seq), original.state().unwrap());
}

#[test]
fn a_tampered_cascade_list_is_divergence() {
    let original = record_session();
    let climb = original
        .events()
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EventKind::Patch)
        .nth(2)
        .unwrap();
    let mut payload = climb.payload.clone();
    payload["cascaded"] = json!([]);
    let dir = std::env::temp_dir().join(format!("loam-tamper-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let target = dir.join("tampered.db");
    let _ = std::fs::remove_file(&target);
    let copy = original
        .fork(original.last_seq().unwrap(), &target)
        .unwrap();
    drop(copy);
    let conn = rusqlite::Connection::open(&target).unwrap();
    conn.execute_batch("DROP TRIGGER events_no_update").unwrap();
    conn.execute(
        "UPDATE events SET payload = ?1 WHERE seq = ?2",
        rusqlite::params![payload.to_string(), climb.seq],
    )
    .unwrap();
    drop(conn);
    let tampered = World::open(&target).unwrap();
    match tampered.replay_in_memory() {
        Err(ReplayError::Diverged { seq, reason }) => {
            assert_eq!(seq, climb.seq);
            assert!(reason.contains("cascaded"), "{reason}");
        }
        other => panic!("expected divergence, got {:?}", other.map(|_| ())),
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn replayed_world_hands_out_the_same_next_ids() {
    let mut original = record_session();
    let mut replayed = original.replay_in_memory().unwrap();
    let p = Patch::new(vec![Op::Create {
        handle: None,
        name: "Eel".into(),
        kind: "creature".into(),
        location: Some(2.into()),
        props: Default::default(),
    }]);
    let a = original.apply_patch(&p).unwrap();
    let b = replayed.apply_patch(&p).unwrap();
    assert_eq!(a, b, "same entity id and event seq");
    // Entity 6 was destroyed; AUTOINCREMENT must not reuse it on replay.
    assert_eq!(a.created, vec![7]);
}

/// The original's state as it stood right after event `at`.
fn state_at(at: i64) -> loam::world::WorldState {
    let original = record_session();
    let events = original.events().unwrap();
    let fork = original.fork_in_memory(at).unwrap();
    assert_eq!(fork.events().unwrap(), events[..at as usize].to_vec());
    fork.state().unwrap()
}

#[test]
fn fork_at_n_matches_the_session_at_n() {
    let original = record_session();
    let events = original.events().unwrap();
    // The first patch event is where the abbey exists but nothing happened.
    let first_patch = events
        .iter()
        .find(|e| e.kind == EventKind::Patch)
        .unwrap()
        .seq;
    let state = state_at(first_patch);
    let names: Vec<_> = state.entities.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["Nave", "Crypt", "Rotten door", "You", "Lamp"]);
    assert_eq!(state.entities[2].props["rotten"], json!(true));
    assert_eq!(state.next_entity_id, 6);

    // Every prefix replays cleanly.
    for at in 0..=original.last_seq().unwrap() {
        let fork = original.fork_in_memory(at).unwrap();
        assert_eq!(fork.last_seq().unwrap(), at);
        assert_eq!(fork.forked_at().unwrap(), Some(at));
    }
    assert!(state_at(0).entities.is_empty());
}

#[test]
fn branch_diverges_without_touching_the_parent() {
    let original = record_session();
    let before = original.state().unwrap();
    let parent_events = original.events().unwrap();
    let first_patch = parent_events
        .iter()
        .find(|e| e.kind == EventKind::Patch)
        .unwrap()
        .seq;

    let mut branch = original.fork_in_memory(first_patch).unwrap();
    // In this branch the player burns the door instead of kicking it.
    let a = branch
        .apply_patch(&Patch::new(vec![Op::SetProp {
            entity: 3.into(),
            key: "burnt".into(),
            value: json!(true),
        }]))
        .unwrap();
    assert_eq!(a.event_seq, first_patch + 1, "branch log continues from N");

    assert_eq!(
        branch.entity(3).unwrap().unwrap().props["burnt"],
        json!(true)
    );
    assert_eq!(original.state().unwrap(), before);
    assert_eq!(original.events().unwrap(), parent_events);
    assert!(
        !original
            .entity(3)
            .unwrap()
            .unwrap()
            .props
            .contains_key("burnt")
    );

    // A branch replays to itself and remembers where it forked.
    let again = branch.replay_in_memory().unwrap();
    assert_eq!(again.state().unwrap(), branch.state().unwrap());
    assert_eq!(again.forked_at().unwrap(), Some(first_patch));
}

#[test]
fn fork_to_file_persists_the_branch() {
    let dir = std::env::temp_dir().join(format!("loam-fork-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("branch.db");
    let _ = std::fs::remove_file(&path);

    let original = record_session();
    let at = original.last_seq().unwrap() - 1;
    let state = original.fork_in_memory(at).unwrap().state().unwrap();
    drop(original.fork(at, &path).unwrap());

    let reopened = World::open(&path).unwrap();
    assert_eq!(reopened.state().unwrap(), state);
    assert_eq!(reopened.forked_at().unwrap(), Some(at));

    // The file now holds a world, so it can't be a fork target again.
    assert!(matches!(
        original.fork(at, &path),
        Err(ReplayError::TargetNotEmpty)
    ));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn fork_point_must_be_inside_the_log() {
    let original = record_session();
    let last = original.last_seq().unwrap();
    for at in [-1, last + 1] {
        match original.fork_in_memory(at) {
            Err(ReplayError::ForkPointOutOfRange { at: a, last: l }) => {
                assert_eq!((a, l), (at, last));
            }
            other => panic!("expected out of range, got {:?}", other.map(|_| ())),
        }
    }
}

/// A quest concerning two things and a rule pinned to one of them; then a
/// destroy that cascades both the concern and the pin, retiring the rule.
#[test]
fn concerns_and_pins_replay_and_fork() {
    use loam::rules::{self, Condition, RuleStatus};
    let mut w = World::open_in_memory().unwrap();
    let a = w
        .apply_patch(&Patch::new(
            serde_json::from_value(json!([
                {"op": "create", "handle": "nave", "name": "Nave", "kind": "room"},
                {"op": "create", "handle": "door", "name": "Rotten door", "kind": "door",
                 "location": "nave"},
                {"op": "create", "handle": "bell", "name": "Bell", "kind": "item",
                 "location": "nave"}
            ]))
            .unwrap(),
        ))
        .unwrap();
    let (door, bell) = (a.handles["door"], a.handles["bell"]);
    let q = loam::quest::spawn(&mut w, "Ring the bell", "A monk asks.", &[bell]).unwrap();
    loam::quest::reveal_goal(&mut w, q, "The bell rings beyond the door.", &[door]).unwrap();
    let def = serde_json::from_value(json!({
        "name": "Kick the door", "trigger": "kick <target>",
        "roles": {"target": "the door"},
        "conditions": [{"check": "entity", "role": "target", "id": door}],
        "effects": [{"op": "set_prop", "entity": "target", "key": "broken", "value": true}]
    }))
    .unwrap();
    let (rule, _) = rules::mint(&mut w, def, "").unwrap().unwrap();
    let before = w.last_seq().unwrap();
    let destroy = w
        .apply_patch(&Patch::new(vec![Op::Destroy {
            entity: door.into(),
        }]))
        .unwrap();

    // The quest still loads, concerning only the bell; the rule is retired.
    assert_eq!(loam::quest::quest(&w, q).unwrap().unwrap().concerns, [bell]);
    let retired = rules::get(&w, rule.id).unwrap().unwrap();
    assert_eq!(retired.status, RuleStatus::Retired);
    assert!(retired.def.conditions.contains(&Condition::Entity {
        role: "target".into(),
        id: None
    }));

    // Replay and forks on either side of the destroy reproduce the store.
    let replayed = w.replay_in_memory().unwrap();
    assert_eq!(replayed.state().unwrap(), w.state().unwrap());
    assert_eq!(replayed.events().unwrap(), w.events().unwrap());
    let fork = w.fork_in_memory(before).unwrap();
    assert!(rules::get(&fork, rule.id).unwrap().unwrap().is_active());
    assert_eq!(
        loam::quest::quest(&fork, q).unwrap().unwrap().concerns,
        [door, bell]
    );
    // The retirement is part of the destroy's patch: forking at that event
    // already holds it.
    let at_destroy = w.fork_in_memory(destroy.event_seq).unwrap();
    assert_eq!(at_destroy.state().unwrap(), w.state().unwrap());
    assert_eq!(w.last_seq().unwrap(), destroy.event_seq + 1);
    let change = &w.events().unwrap()[destroy.event_seq as usize];
    assert_eq!(change.kind, EventKind::RuleChange);
    assert_eq!(change.payload["reason"], json!("pinned entity destroyed"));
}
