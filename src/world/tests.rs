use super::*;
use serde_json::json;

fn create(handle: &str, name: &str, kind: &str, location: Option<EntityRef>) -> Op {
    Op::Create {
        handle: Some(handle.into()),
        name: name.into(),
        kind: kind.into(),
        location,
        props: Map::new(),
    }
}

fn mv(entity: impl Into<EntityRef>, to: Option<EntityRef>) -> Op {
    Op::Move {
        entity: entity.into(),
        to,
    }
}

/// A cellar containing a chest containing a key.
fn seeded() -> (World, EntityId, EntityId, EntityId) {
    let mut w = World::open_in_memory().unwrap();
    let a = w
        .apply_patch(&Patch::new(vec![
            create("cellar", "Cellar", "room", None),
            create("chest", "Chest", "container", Some("cellar".into())),
            create("key", "Iron key", "item", Some("chest".into())),
        ]))
        .unwrap();
    let h = &a.handles;
    (w, h["cellar"], h["chest"], h["key"])
}

fn rejected(w: &mut World, ops: Vec<Op>) -> Rejection {
    match w.apply_patch(&Patch::new(ops)) {
        Err(ApplyError::Rejected(r)) => r,
        other => panic!("expected rejection, got {other:?}"),
    }
}

/// Snapshot of every entity, to prove a rejected patch changed nothing.
fn snapshot(w: &World) -> Vec<Entity> {
    let ids: Vec<EntityId> = w
        .conn
        .prepare("SELECT id FROM entities ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    ids.into_iter()
        .map(|id| w.entity(id).unwrap().unwrap())
        .collect()
}

// ---- valid patches ----

#[test]
fn create_builds_containment_tree_with_props() {
    let mut w = World::open_in_memory().unwrap();
    let mut props = Map::new();
    props.insert("rotten".into(), json!(true));
    props.insert("hp".into(), json!(3));
    let a = w
        .apply_patch(&Patch::new(vec![
            create("hall", "Hall", "room", None),
            Op::Create {
                handle: Some("door".into()),
                name: "Rotten door".into(),
                kind: "door".into(),
                location: Some("hall".into()),
                props,
            },
            Op::Create {
                handle: None,
                name: "Dust".into(),
                kind: "scenery".into(),
                location: Some("hall".into()),
                props: Map::new(),
            },
        ]))
        .unwrap();
    assert_eq!(a.created.len(), 3);
    let (hall, door) = (a.handles["hall"], a.handles["door"]);
    assert_eq!(a.created[..2], [hall, door]);

    let d = w.entity(door).unwrap().unwrap();
    assert_eq!(d.name, "Rotten door");
    assert_eq!(d.kind, "door");
    assert_eq!(d.location, Some(hall));
    assert_eq!(d.props["rotten"], json!(true));
    assert_eq!(d.props["hp"], json!(3));
    assert_eq!(w.contents(Some(hall)).unwrap(), vec![door, a.created[2]]);
    assert_eq!(w.contents(None).unwrap(), vec![hall]);
}

#[test]
fn move_set_remove_rename_destroy() {
    let (mut w, cellar, chest, key) = seeded();
    w.apply_patch(&Patch::new(vec![
        mv(key, Some(cellar.into())),
        Op::SetProp {
            entity: chest.into(),
            key: "open".into(),
            value: json!(true),
        },
        Op::SetProp {
            entity: chest.into(),
            key: "open".into(),
            value: json!("ajar"),
        },
        Op::Rename {
            entity: key.into(),
            name: "Rusty key".into(),
        },
    ]))
    .unwrap();
    let k = w.entity(key).unwrap().unwrap();
    assert_eq!((k.location, k.name.as_str()), (Some(cellar), "Rusty key"));
    assert_eq!(
        w.entity(chest).unwrap().unwrap().props["open"],
        json!("ajar")
    );

    w.apply_patch(&Patch::new(vec![
        Op::RemoveProp {
            entity: chest.into(),
            key: "open".into(),
        },
        Op::Destroy {
            entity: chest.into(),
        },
        mv(key, None),
    ]))
    .unwrap();
    assert!(w.entity(chest).unwrap().is_none());
    assert_eq!(w.entity(key).unwrap().unwrap().location, None);
    let orphan_props: i64 = w
        .conn
        .query_row(
            "SELECT COUNT(*) FROM props WHERE entity_id = ?1",
            [chest],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(orphan_props, 0);
}

#[test]
fn moving_out_of_a_subtree_then_inverting_it_is_not_a_cycle() {
    // Ops apply in order: once the key leaves the cellar's subtree, the
    // cellar may move under it.
    let (mut w, cellar, _chest, key) = seeded();
    w.apply_patch(&Patch::new(vec![
        mv(key, None),
        mv(cellar, Some(key.into())),
    ]))
    .unwrap();
    assert_eq!(w.entity(cellar).unwrap().unwrap().location, Some(key));
}

#[test]
fn patch_parses_from_director_json() {
    let p: Patch = serde_json::from_value(json!({"ops": [
        {"op": "create", "handle": "crow", "name": "Crow", "kind": "npc", "location": 1,
         "props": {"mood": "wary"}},
        {"op": "move", "entity": "crow", "to": null},
        {"op": "set_prop", "entity": 1, "key": "lit", "value": false}
    ]}))
    .unwrap();
    assert_eq!(
        p.ops[1],
        Op::Move {
            entity: EntityRef::Handle("crow".into()),
            to: None
        }
    );
    // Round-trips through the log payload format.
    assert_eq!(serde_json::from_value::<Patch>(json!(p)).unwrap(), p);
}

// ---- rejections ----

#[test]
fn rejects_missing_entity_id() {
    let (mut w, ..) = seeded();
    let r = rejected(
        &mut w,
        vec![Op::SetProp {
            entity: 999.into(),
            key: "x".into(),
            value: json!(1),
        }],
    );
    assert_eq!(r.op, 0);
    assert_eq!(r.reason, "entity #999 does not exist");
}

#[test]
fn rejects_unknown_handle() {
    let (mut w, ..) = seeded();
    let r = rejected(&mut w, vec![mv("ghost", None)]);
    assert!(r.reason.contains("unknown handle 'ghost'"), "{}", r.reason);
}

#[test]
fn rejects_missing_destination() {
    let (mut w, _, _, key) = seeded();
    let r = rejected(&mut w, vec![mv(key, Some(404.into()))]);
    assert_eq!(r.reason, "entity #404 does not exist");
    let r = rejected(&mut w, vec![create("x", "X", "item", Some(404.into()))]);
    assert_eq!(r.reason, "entity #404 does not exist");
}

#[test]
fn rejects_reference_to_destroyed_entity() {
    let (mut w, _, _, key) = seeded();
    let r = rejected(
        &mut w,
        vec![
            Op::Destroy { entity: key.into() },
            Op::Rename {
                entity: key.into(),
                name: "x".into(),
            },
        ],
    );
    assert_eq!(r.op, 1);
    assert!(r.reason.contains("does not exist"));
}

#[test]
fn rejects_double_location_from_two_moves() {
    let (mut w, cellar, chest, key) = seeded();
    let r = rejected(
        &mut w,
        vec![mv(key, Some(cellar.into())), mv(key, Some(chest.into()))],
    );
    assert_eq!(r.op, 1);
    assert!(r.reason.contains("two places"), "{}", r.reason);
}

#[test]
fn rejects_double_location_from_create_then_move() {
    let (mut w, cellar, chest, _) = seeded();
    let r = rejected(
        &mut w,
        vec![
            create("coin", "Coin", "item", Some(chest.into())),
            mv("coin", Some(cellar.into())),
        ],
    );
    assert_eq!(r.op, 1);
    assert!(
        r.reason.contains("'coin' would be in two places"),
        "{}",
        r.reason
    );
}

#[test]
fn rejects_duplicate_handle() {
    let mut w = World::open_in_memory().unwrap();
    let r = rejected(
        &mut w,
        vec![
            create("a", "A", "room", None),
            create("a", "A2", "room", None),
        ],
    );
    assert_eq!(r.op, 1);
    assert!(r.reason.contains("already used"));
}

#[test]
fn rejects_self_containment() {
    let (mut w, _, chest, _) = seeded();
    let r = rejected(&mut w, vec![mv(chest, Some(chest.into()))]);
    assert!(r.reason.contains("cannot contain itself"), "{}", r.reason);
}

#[test]
fn rejects_containment_cycle_through_descendant() {
    let (mut w, cellar, _, key) = seeded();
    let r = rejected(&mut w, vec![mv(cellar, Some(key.into()))]);
    assert!(r.reason.contains("containment cycle"), "{}", r.reason);
}

#[test]
fn rejects_destroying_non_empty_container() {
    let (mut w, _, chest, _) = seeded();
    let r = rejected(
        &mut w,
        vec![Op::Destroy {
            entity: chest.into(),
        }],
    );
    assert!(r.reason.contains("still contains 1 entity"), "{}", r.reason);
}

#[test]
fn rejected_patch_changes_nothing() {
    let (mut w, cellar, chest, key) = seeded();
    let before = snapshot(&w);
    rejected(
        &mut w,
        vec![
            create("rat", "Rat", "npc", Some(cellar.into())),
            Op::SetProp {
                entity: chest.into(),
                key: "open".into(),
                value: json!(true),
            },
            mv(key, Some(cellar.into())),
            mv(cellar, Some(key.into())), // cycle: whole patch must roll back
        ],
    );
    assert_eq!(snapshot(&w), before);
}

// ---- event log ----

#[test]
fn every_applied_patch_is_logged_once() {
    let (mut w, cellar, _, key) = seeded();
    let p = Patch::new(vec![mv(key, Some(cellar.into()))]);
    let a = w.apply_patch(&p).unwrap();

    let patches: Vec<Event> = w
        .events()
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EventKind::Patch)
        .collect();
    assert_eq!(patches.len(), 2, "seed patch + move");
    let last = patches.last().unwrap();
    assert_eq!(last.seq, a.event_seq);
    assert_eq!(
        serde_json::from_value::<Patch>(last.payload["patch"].clone()).unwrap(),
        p
    );

    let seed = &patches[0].payload;
    assert_eq!(seed["handles"]["key"], json!(key));
    assert_eq!(seed["created"].as_array().unwrap().len(), 3);
}

#[test]
fn rejected_patches_are_logged_with_reason() {
    let (mut w, ..) = seeded();
    let r = rejected(&mut w, vec![mv(77, None)]);
    let last = w.events().unwrap().pop().unwrap();
    assert_eq!(last.kind, EventKind::PatchRejected);
    assert_eq!(last.payload["reason"], json!(r.reason));
    assert_eq!(last.payload["op"], json!(0));
    assert_eq!(
        w.events()
            .unwrap()
            .iter()
            .filter(|e| e.kind == EventKind::Patch)
            .count(),
        1
    );
}

#[test]
fn log_accepts_every_kind_in_order() {
    let w = World::open_in_memory().unwrap();
    for (i, k) in EventKind::ALL.into_iter().enumerate() {
        let seq = w.log_event(k, &json!({"i": i})).unwrap();
        assert_eq!(seq, i as i64 + 1);
    }
    let events = w.events().unwrap();
    assert_eq!(
        events.iter().map(|e| e.kind).collect::<Vec<_>>(),
        EventKind::ALL
    );
    assert!(events.iter().all(|e| e.at.ends_with('Z')));
    assert_eq!(w.events_since(5).unwrap().len(), EventKind::ALL.len() - 5);
}

#[test]
fn log_is_append_only() {
    let w = World::open_in_memory().unwrap();
    w.log_event(EventKind::Input, &json!("kick the door"))
        .unwrap();
    assert!(
        w.conn
            .execute("UPDATE events SET payload = '\"x\"'", [])
            .is_err()
    );
    assert!(w.conn.execute("DELETE FROM events", []).is_err());
    assert_eq!(w.events().unwrap()[0].payload, json!("kick the door"));
}

#[test]
fn world_persists_across_reopen() {
    let dir = std::env::temp_dir().join(format!("loam-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("world.db");
    let _ = std::fs::remove_file(&path);
    let key = {
        let mut w = World::open(&path).unwrap();
        w.apply_patch(&Patch::new(vec![create("k", "Key", "item", None)]))
            .unwrap()
            .handles["k"]
    };
    let w = World::open(&path).unwrap();
    assert_eq!(w.entity(key).unwrap().unwrap().name, "Key");
    assert_eq!(w.events().unwrap().len(), 1);
    drop(w);
    std::fs::remove_dir_all(&dir).unwrap();
}
