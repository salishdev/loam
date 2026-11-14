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

/// A `create` of something marked `container: true`.
fn create_container(handle: &str, name: &str, location: Option<EntityRef>) -> Op {
    Op::Create {
        handle: Some(handle.into()),
        name: name.into(),
        kind: "container".into(),
        location,
        props: Map::from_iter([(CONTAINER.to_owned(), json!(true))]),
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
        .apply_engine_patch(&Patch::new(vec![
            create("cellar", "Cellar", "room", None),
            create_container("chest", "Chest", Some("cellar".into())),
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
        .apply_engine_patch(&Patch::new(vec![
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
        Op::SetProp {
            entity: key.into(),
            key: CONTAINER.into(),
            value: json!(true),
        },
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
            create("a", "A", "thing", None),
            create("a", "A2", "thing", None),
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

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct Mood {
    level: i64,
    #[serde(default)]
    why: String,
}

impl Component for Mood {
    const KEY: &'static str = "_mood";
}

#[test]
fn components_round_trip_through_engine_patches() {
    let (mut w, cellar, chest, key) = seeded();
    assert_eq!(w.get::<Mood>(chest).unwrap(), None);
    assert_eq!(w.query::<Mood>().unwrap(), vec![]);

    let calm = Mood {
        level: 1,
        why: "quiet".into(),
    };
    let a = w.set(key, &calm).unwrap();
    // Written as an ordinary engine-owned prop, logged as an engine patch.
    assert_eq!(
        w.entity(key).unwrap().unwrap().props["_mood"],
        json!({"level": 1, "why": "quiet"})
    );
    let event = w.events().unwrap().pop().unwrap();
    assert_eq!(event.seq, a.event_seq);
    assert_eq!(event.payload["by"], json!("engine"));

    let tense = Mood {
        level: 3,
        why: String::new(),
    };
    w.apply_engine_patch(&Patch::new(vec![set_op(cellar, &tense)]))
        .unwrap();
    assert_eq!(w.get::<Mood>(key).unwrap(), Some(calm.clone()));
    assert_eq!(
        w.entity(key).unwrap().unwrap().get::<Mood>().unwrap(),
        Some(calm.clone())
    );
    // Query returns every carrier of the component in id order.
    assert_eq!(
        w.query::<Mood>().unwrap(),
        vec![(cellar, tense), (key, calm)]
    );

    w.apply_engine_patch(&Patch::new(vec![remove_op::<Mood>(cellar)]))
        .unwrap();
    assert_eq!(w.get::<Mood>(cellar).unwrap(), None);

    // A create can carry a component from the start.
    let a = w
        .apply_engine_patch(&Patch::new(vec![Op::Create {
            handle: None,
            name: "Ghost".into(),
            kind: "npc".into(),
            location: Some(cellar.into()),
            props: Map::from_iter([prop(&Mood {
                level: 9,
                why: "restless".into(),
            })]),
        }]))
        .unwrap();
    assert_eq!(w.get::<Mood>(a.created[0]).unwrap().unwrap().level, 9);

    // The director still can't write it: the key is engine-owned.
    let r = rejected(
        &mut w,
        vec![set_op(
            chest,
            &Mood {
                level: 0,
                why: String::new(),
            },
        )],
    );
    assert!(r.reason.contains("engine-owned"), "{}", r.reason);

    // Replay reproduces components like any prop.
    assert_eq!(
        w.replay_in_memory().unwrap().state().unwrap(),
        w.state().unwrap()
    );
}

#[test]
fn a_malformed_component_is_reported_as_corrupt() {
    let (mut w, _, chest, _) = seeded();
    w.apply_engine_patch(&Patch::new(vec![Op::SetProp {
        entity: chest.into(),
        key: "_mood".into(),
        value: json!("grumpy"),
    }]))
    .unwrap();
    let err = w.get::<Mood>(chest).unwrap_err().to_string();
    assert!(
        err.contains(&format!("entity {chest}")) && err.contains("_mood"),
        "{err}"
    );
    assert!(w.query::<Mood>().is_err());
    assert!(w.entity(chest).unwrap().unwrap().get::<Mood>().is_err());
}

fn temp_store(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("loam-world-test-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("save.db");
    let _ = std::fs::remove_file(&path);
    path
}

fn schema_version(w: &World) -> String {
    w.conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .unwrap()
}

#[test]
fn new_stores_record_the_schema_version_and_reopen() {
    let path = temp_store("fresh");
    let mut w = World::open(&path).unwrap();
    assert_eq!(schema_version(&w), SCHEMA_VERSION.to_string());
    w.apply_engine_patch(&Patch::new(vec![create("hall", "Hall", "room", None)]))
        .unwrap();
    drop(w);
    let w = World::open(&path).unwrap();
    assert_eq!(w.entities_of_kind("room").unwrap().len(), 1);
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn stores_from_other_schema_versions_are_refused() {
    // A save from before versioning: data but no version row.
    let path = temp_store("unversioned");
    let mut w = World::open(&path).unwrap();
    w.apply_engine_patch(&Patch::new(vec![create("hall", "Hall", "room", None)]))
        .unwrap();
    w.conn
        .execute("DELETE FROM meta WHERE key = 'schema_version'", [])
        .unwrap();
    drop(w);
    let err = World::open(&path).err().expect("refused").to_string();
    assert!(
        err.contains("older loam") && err.contains("new game"),
        "{err}"
    );

    for (stored, made_by) in [
        ("1", "older loam (schema 1)"),
        ("99", "newer loam (schema 99)"),
    ] {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('schema_version', ?1)",
            [stored],
        )
        .unwrap();
        drop(conn);
        let err = World::open(&path).err().expect("refused").to_string();
        assert!(err.contains(made_by), "{err}");
    }
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

// ---- relations ----

fn relate(subject: impl Into<EntityRef>, rel: &str, object: impl Into<EntityRef>) -> Op {
    Op::Relate {
        subject: subject.into(),
        rel: rel.into(),
        object: object.into(),
        props: Map::new(),
    }
}

fn unrelate(subject: impl Into<EntityRef>, rel: &str, object: impl Into<EntityRef>) -> Op {
    Op::Unrelate {
        subject: subject.into(),
        rel: rel.into(),
        object: object.into(),
    }
}

fn row(subject: EntityId, rel: &str, object: EntityId, props: Value) -> Relation {
    Relation {
        subject,
        rel: rel.into(),
        object,
        props: props.as_object().unwrap().clone(),
    }
}

#[test]
fn relate_upserts_and_reads_both_ways_with_filters() {
    let (mut w, cellar, chest, key) = seeded();
    w.apply_patch(&Patch::new(vec![
        relate(key, "opens", chest),
        Op::Relate {
            subject: chest.into(),
            rel: "guards".into(),
            object: cellar.into(),
            props: json!({"since": "dawn"}).as_object().unwrap().clone(),
        },
    ]))
    .unwrap();
    assert_eq!(
        w.relations_from(key, None).unwrap(),
        vec![row(key, "opens", chest, json!({}))]
    );
    assert_eq!(
        w.relations_to(chest, Some("opens")).unwrap(),
        vec![row(key, "opens", chest, json!({}))]
    );
    assert!(w.relations_to(chest, Some("guards")).unwrap().is_empty());

    // Relating again replaces the props; it doesn't add a second row.
    w.apply_patch(&Patch::new(vec![Op::Relate {
        subject: chest.into(),
        rel: "guards".into(),
        object: cellar.into(),
        props: json!({"since": "dusk"}).as_object().unwrap().clone(),
    }]))
    .unwrap();
    assert_eq!(
        w.relations_from(chest, None).unwrap(),
        vec![row(chest, "guards", cellar, json!({"since": "dusk"}))]
    );
    assert_eq!(w.relations().unwrap().len(), 2);

    // Directional: the reverse fact doesn't hold.
    let r = rejected(&mut w, vec![unrelate(cellar, "guards", chest)]);
    assert!(r.reason.contains("no such relation"), "{}", r.reason);

    w.apply_patch(&Patch::new(vec![unrelate(chest, "guards", cellar)]))
        .unwrap();
    assert!(w.relations_from(chest, None).unwrap().is_empty());
}

#[test]
fn relate_takes_handles_from_earlier_creates() {
    let (mut w, cellar, _, _) = seeded();
    let a = w
        .apply_patch(&Patch::new(vec![
            create("hound", "Hound", "npc", Some(cellar.into())),
            relate("hound", "guards", cellar),
        ]))
        .unwrap();
    let hound = a.handles["hound"];
    assert_eq!(
        w.relations_from(hound, None).unwrap(),
        vec![row(hound, "guards", cellar, json!({}))]
    );
}

#[test]
fn relation_ops_are_validated() {
    let (mut w, cellar, chest, _) = seeded();
    let before = w.state().unwrap();
    for (ops, want) in [
        (vec![relate(99, "guards", chest)], "#99 does not exist"),
        (vec![relate(chest, "guards", 99)], "#99 does not exist"),
        (vec![relate("nope", "guards", chest)], "unknown handle"),
        (vec![relate(chest, "  ", cellar)], "needs a name"),
        (vec![unrelate(chest, "guards", cellar)], "no such relation"),
        // An earlier op's relation is rolled back with the patch.
        (
            vec![
                relate(chest, "guards", cellar),
                unrelate(chest, "owns", cellar),
            ],
            "no such relation",
        ),
    ] {
        let r = rejected(&mut w, ops);
        assert!(r.reason.contains(want), "{} !~ {want}", r.reason);
    }
    assert_eq!(w.state().unwrap(), before);
}

#[test]
fn engine_owned_relation_names_are_refused_for_the_director_only() {
    let (mut w, cellar, chest, _) = seeded();
    let r = rejected(&mut w, vec![relate(chest, "_pins", cellar)]);
    assert!(r.reason.contains("engine-owned"), "{}", r.reason);
    w.apply_engine_patch(&Patch::new(vec![relate(chest, "_pins", cellar)]))
        .unwrap();
    let r = rejected(&mut w, vec![unrelate(chest, "_pins", cellar)]);
    assert!(r.reason.contains("engine-owned"), "{}", r.reason);
    w.apply_engine_patch(&Patch::new(vec![unrelate(chest, "_pins", cellar)]))
        .unwrap();
    assert!(w.relations().unwrap().is_empty());
}

#[test]
fn destroy_cascades_relations_both_ways_and_logs_them() {
    let (mut w, cellar, chest, key) = seeded();
    let hall = w
        .apply_engine_patch(&Patch::new(vec![create("hall", "Hall", "room", None)]))
        .unwrap()
        .created[0];
    w.apply_patch(&Patch::new(vec![
        relate(key, "opens", chest),
        Op::Relate {
            subject: hall.into(),
            rel: "beside".into(),
            object: key.into(),
            props: json!({"far": false}).as_object().unwrap().clone(),
        },
        relate(hall, "beside", cellar),
    ]))
    .unwrap();

    let applied = w
        .apply_patch(&Patch::new(vec![
            Op::SetProp {
                entity: chest.into(),
                key: "x".into(),
                value: json!(1),
            },
            Op::Destroy { entity: key.into() },
        ]))
        .unwrap();
    assert_eq!(
        w.relations().unwrap(),
        vec![row(hall, "beside", cellar, json!({}))]
    );
    let event = w.events().unwrap().pop().unwrap();
    assert_eq!(event.seq, applied.event_seq);
    assert_eq!(
        event.payload["cascaded"],
        json!([
            {"op": 1, "subject": key, "rel": "opens", "object": chest, "props": {}},
            {"op": 1, "subject": hall, "rel": "beside", "object": key, "props": {"far": false}},
        ])
    );

    // A destroy with nothing to cascade records no list.
    let applied = w
        .apply_patch(&Patch::new(vec![Op::Destroy {
            entity: chest.into(),
        }]))
        .unwrap();
    let event = w.events().unwrap().pop().unwrap();
    assert_eq!(event.seq, applied.event_seq);
    assert!(event.payload.get("cascaded").is_none());
}

#[test]
fn destroy_still_refuses_a_non_empty_container_and_keeps_its_relations() {
    let (mut w, cellar, chest, key) = seeded();
    w.apply_patch(&Patch::new(vec![relate(chest, "in", cellar)]))
        .unwrap();
    let r = rejected(
        &mut w,
        vec![Op::Destroy {
            entity: chest.into(),
        }],
    );
    assert!(r.reason.contains("still contains"), "{}", r.reason);
    assert_eq!(w.relations().unwrap().len(), 1);
    assert!(w.entity(key).unwrap().is_some());
}

#[test]
fn a_note_cannot_override_the_cascaded_list() {
    let (mut w, cellar, chest, key) = seeded();
    w.apply_patch(&Patch::new(vec![relate(key, "opens", chest)]))
        .unwrap();
    let note = json!({"cascaded": [], "rule": {"id": 1}});
    w.apply_patch_noted(
        &Patch::new(vec![
            mv(key, Some(cellar.into())),
            Op::Destroy { entity: key.into() },
        ]),
        note.as_object().unwrap(),
    )
    .unwrap();
    let event = w.events().unwrap().pop().unwrap();
    assert_eq!(event.payload["cascaded"].as_array().unwrap().len(), 1);
    assert_eq!(event.payload["rule"]["id"], 1);
}

#[test]
fn relation_ops_touch_both_ends() {
    let (mut w, cellar, chest, _) = seeded();
    w.apply_patch(&Patch::new(vec![relate(chest, "guards", cellar)]))
        .unwrap();
    let event = w.events().unwrap().pop().unwrap();
    let touches = event.patch_touches().unwrap();
    assert_eq!(touches[0].1, vec![chest, cellar]);
}

fn exit(subject: impl Into<EntityRef>, object: impl Into<EntityRef>, props: Value) -> Op {
    let Value::Object(props) = props else {
        panic!("props must be an object")
    };
    Op::Relate {
        subject: subject.into(),
        rel: EXIT_REL.into(),
        object: object.into(),
        props,
    }
}

#[test]
fn exits_join_two_rooms_with_distinct_dirs() {
    let (mut w, cellar, chest, _) = seeded();
    let a = w
        .apply_engine_patch(&Patch::new(vec![
            create("hall", "Hall", ROOM_KIND, None),
            create("yard", "Yard", ROOM_KIND, None),
            exit(cellar, "hall", json!({"dir": "up", "label": "Hall"})),
            exit("hall", cellar, json!({"dir": "down", "label": "Cellar"})),
        ]))
        .unwrap();
    let (hall, yard) = (a.handles["hall"], a.handles["yard"]);

    // Both ends must be rooms.
    let r = rejected(
        &mut w,
        vec![exit(cellar, chest, json!({"dir": "in", "label": "x"}))],
    );
    assert!(
        r.reason.contains("joins two rooms") && r.reason.contains("object"),
        "{r}"
    );
    let r = rejected(
        &mut w,
        vec![exit(chest, cellar, json!({"dir": "out", "label": "x"}))],
    );
    assert!(r.reason.contains("subject"), "{r}");
    // A dir is required.
    for props in [
        json!({"label": "x"}),
        json!({"dir": " ", "label": "x"}),
        json!({"dir": 3}),
    ] {
        let r = rejected(&mut w, vec![exit(cellar, yard, props)]);
        assert!(r.reason.contains("dir"), "{r}");
    }
    // No two exits from one room share a dir, for the engine too.
    let r = rejected(
        &mut w,
        vec![exit(cellar, yard, json!({"dir": "up", "label": "Yard"}))],
    );
    assert!(r.reason.contains("already has an exit \"up\""), "{r}");
    match w.apply_engine_patch(&Patch::new(vec![exit(
        cellar,
        yard,
        json!({"dir": "up", "label": "Yard"}),
    )])) {
        Err(ApplyError::Rejected(r)) => assert!(r.reason.contains("already"), "{r}"),
        other => panic!("expected rejection, got {other:?}"),
    }
    // A second exit to the same room is refused rather than renaming the
    // first; renaming takes an unrelate first.
    let r = rejected(
        &mut w,
        vec![exit(cellar, hall, json!({"dir": "stairs", "label": "Hall"}))],
    );
    assert!(
        r.reason.contains("already has an exit \"up\" to") && r.reason.contains("unrelate"),
        "{r}"
    );
    assert_eq!(
        w.relations_from(cellar, Some(EXIT_REL)).unwrap()[0].props["dir"],
        json!("up")
    );
    w.apply_patch(&Patch::new(vec![
        Op::Unrelate {
            subject: cellar.into(),
            rel: EXIT_REL.into(),
            object: hall.into(),
        },
        exit(cellar, hall, json!({"dir": "stairs", "label": "Hall"})),
        Op::Unrelate {
            subject: cellar.into(),
            rel: EXIT_REL.into(),
            object: hall.into(),
        },
        exit(cellar, hall, json!({"dir": "up", "label": "Hall"})),
    ]))
    .unwrap();
    // Relabelling the same exit is an upsert, not a clash.
    w.apply_patch(&Patch::new(vec![exit(
        cellar,
        hall,
        json!({"dir": "up", "label": "Great hall"}),
    )]))
    .unwrap();
    assert_eq!(
        w.relations_from(cellar, Some(EXIT_REL)).unwrap()[0].props["label"],
        json!("Great hall")
    );
    // A different dir to another room is fine.
    w.apply_patch(&Patch::new(vec![exit(
        cellar,
        yard,
        json!({"dir": "east", "label": "Yard"}),
    )]))
    .unwrap();
    assert_eq!(w.relations_from(cellar, Some(EXIT_REL)).unwrap().len(), 2);
}

// ---- markers and reserved kinds ----

fn create_with(handle: &str, kind: &str, location: Option<EntityRef>, props: Value) -> Op {
    Op::Create {
        handle: Some(handle.into()),
        name: handle.into(),
        kind: kind.into(),
        location,
        props: props.as_object().unwrap().clone(),
    }
}

/// A cellar (room) holding a plain stone and a key; a person who acts; a
/// player; and a carrier pouch.
fn holders() -> (World, BTreeMap<String, EntityId>) {
    let mut w = World::open_in_memory().unwrap();
    let a = w
        .apply_engine_patch(&Patch::new(vec![
            create("cellar", "Cellar", ROOM_KIND, None),
            create("stone", "Stone", "stone", Some("cellar".into())),
            create("key", "Key", "key", Some("cellar".into())),
            create_with(
                "ferry",
                "ferryman",
                Some("cellar".into()),
                json!({ACTS: true}),
            ),
            create("you", "You", PLAYER_KIND, Some("cellar".into())),
            create_with("pouch", "pouch", None, json!({CARRIER_PROP: true})),
        ]))
        .unwrap();
    (w, a.handles)
}

#[test]
fn moving_or_creating_into_a_non_container_is_refused_for_every_authority() {
    let (mut w, h) = holders();
    let (stone, key) = (h["stone"], h["key"]);
    let before = snapshot(&w);
    for by_engine in [false, true] {
        for ops in [
            vec![mv(key, Some(stone.into()))],
            vec![create("gem", "Gem", "gem", Some(stone.into()))],
        ] {
            let patch = Patch::new(ops);
            let result = if by_engine {
                w.apply_engine_patch(&patch)
            } else {
                w.apply_patch(&patch)
            };
            match result {
                Err(ApplyError::Rejected(r)) => {
                    assert!(r.reason.contains("not a container"), "{}", r.reason)
                }
                other => panic!("expected rejection, got {other:?}"),
            }
        }
    }
    assert_eq!(snapshot(&w), before);
    // Top level is always allowed.
    w.apply_patch(&Patch::new(vec![mv(key, None)])).unwrap();
}

#[test]
fn rooms_players_carriers_actors_and_containers_hold_things() {
    let (mut w, h) = holders();
    let key = h["key"];
    for holder in ["cellar", "ferry", "you", "pouch"] {
        w.apply_engine_patch(&Patch::new(vec![mv(key, Some(h[holder].into()))]))
            .unwrap();
        assert_eq!(w.entity(key).unwrap().unwrap().location, Some(h[holder]));
    }
    // An acts being holds things for the director too.
    w.apply_engine_patch(&Patch::new(vec![mv(key, Some(h["cellar"].into()))]))
        .unwrap();
    w.apply_patch(&Patch::new(vec![mv(key, Some(h["ferry"].into()))]))
        .unwrap();
    // Marking the stone a container (in the same patch) lets it hold things.
    w.apply_patch(&Patch::new(vec![
        Op::SetProp {
            entity: h["stone"].into(),
            key: CONTAINER.into(),
            value: json!(true),
        },
        mv(key, Some(h["stone"].into())),
    ]))
    .unwrap();
    // `container: false` is not a container.
    let r = rejected(
        &mut w,
        vec![
            create_with(
                "box",
                "box",
                Some(h["cellar"].into()),
                json!({CONTAINER: false}),
            ),
            create("pin", "Pin", "pin", Some("box".into())),
        ],
    );
    assert_eq!(r.op, 1);
}

#[test]
fn reserved_kinds_are_refused_for_the_director_only() {
    let mut w = World::open_in_memory().unwrap();
    for kind in RESERVED_KINDS {
        let r = rejected(&mut w, vec![create("x", "X", kind, None)]);
        assert!(r.reason.contains("is reserved"), "{}", r.reason);
        w.apply_engine_patch(&Patch::new(vec![create("x", "X", kind, None)]))
            .unwrap();
    }
    // Any other kind is a free label.
    w.apply_patch(&Patch::new(vec![create("x", "X", "npc", None)]))
        .unwrap();
}

#[test]
fn entities_marked_finds_true_markers_only() {
    let mut w = World::open_in_memory().unwrap();
    let a = w
        .apply_patch(&Patch::new(vec![
            create_with("a", "x", None, json!({ACTS: true})),
            create_with("b", "x", None, json!({ACTS: false})),
            create_with("c", "x", None, json!({ACTS: "yes"})),
            create_with("d", "x", None, json!({ACTS: true})),
        ]))
        .unwrap();
    assert_eq!(
        w.entities_marked(ACTS).unwrap(),
        vec![a.handles["a"], a.handles["d"]]
    );
}

// ---- changed_since ---------------------------------------------------------

/// Apply `ops` as the engine and return what `changed_since` reports for it.
fn changed_by(w: &mut World, ops: Vec<Op>) -> Vec<EntityId> {
    let before = w.last_seq().unwrap();
    w.apply_engine_patch(&Patch::new(ops)).unwrap();
    w.changed_since(before).unwrap().into_iter().collect()
}

#[test]
fn changed_since_nothing_is_empty() {
    let (w, ..) = seeded();
    assert!(w.changed_since(w.last_seq().unwrap()).unwrap().is_empty());
}

#[test]
fn a_create_changes_the_new_entity_and_its_location() {
    let (mut w, cellar, chest, key) = seeded();
    let before = w.last_seq().unwrap();
    let a = w
        .apply_patch(&Patch::new(vec![create(
            "coin",
            "Coin",
            "item",
            Some(chest.into()),
        )]))
        .unwrap();
    let changed: Vec<_> = w.changed_since(before).unwrap().into_iter().collect();
    assert_eq!(changed, vec![chest, a.created[0]]);
    // Neither the chest's room nor its other contents.
    assert!(!changed.contains(&cellar) && !changed.contains(&key));
    // A top-level create changes only itself.
    let before = w.last_seq().unwrap();
    let b = w
        .apply_patch(&Patch::new(vec![create("cloud", "Cloud", "item", None)]))
        .unwrap();
    let changed: Vec<_> = w.changed_since(before).unwrap().into_iter().collect();
    assert_eq!(changed, vec![b.created[0]]);
}

#[test]
fn a_move_changes_the_entity_and_its_destination_not_its_origin() {
    let (mut w, cellar, chest, key) = seeded();
    let changed = changed_by(&mut w, vec![mv(key, Some(cellar.into()))]);
    assert_eq!(changed, vec![cellar, key]);
    assert!(!changed.contains(&chest));
}

#[test]
fn prop_and_rename_ops_change_only_their_entity() {
    let (mut w, _, _, key) = seeded();
    let set = Op::SetProp {
        entity: key.into(),
        key: "rusty".into(),
        value: json!(true),
    };
    assert_eq!(changed_by(&mut w, vec![set]), vec![key]);
    let remove = Op::RemoveProp {
        entity: key.into(),
        key: "rusty".into(),
    };
    assert_eq!(changed_by(&mut w, vec![remove]), vec![key]);
    let rename = Op::Rename {
        entity: key.into(),
        name: "Rusty key".into(),
    };
    assert_eq!(changed_by(&mut w, vec![rename]), vec![key]);
}

#[test]
fn a_destroy_changes_the_destroyed_entity() {
    let (mut w, _, _, key) = seeded();
    assert_eq!(
        changed_by(&mut w, vec![Op::Destroy { entity: key.into() }]),
        vec![key]
    );
}

#[test]
fn relate_and_unrelate_change_both_ends() {
    let (mut w, cellar, chest, _) = seeded();
    assert_eq!(
        changed_by(&mut w, vec![relate(chest, "guards", cellar)]),
        vec![cellar, chest]
    );
    assert_eq!(
        changed_by(&mut w, vec![unrelate(chest, "guards", cellar)]),
        vec![cellar, chest]
    );
}

#[test]
fn an_engine_owned_relation_changes_only_its_subject() {
    let (mut w, _, chest, key) = seeded();
    assert_eq!(
        changed_by(&mut w, vec![relate(chest, "_watches", key)]),
        vec![chest]
    );
    assert_eq!(
        changed_by(&mut w, vec![unrelate(chest, "_watches", key)]),
        vec![chest]
    );
}

#[test]
fn a_destroy_changes_both_ends_of_every_cascaded_relation() {
    let (mut w, cellar, chest, key) = seeded();
    let a = w
        .apply_engine_patch(&Patch::new(vec![
            create("hound", "Hound", "npc", None),
            relate(key, "opens", chest),
            relate("hound", "wants", key),
            relate(cellar, "_watches", key),
        ]))
        .unwrap();
    let hound = a.handles["hound"];
    let mut want = vec![cellar, chest, key, hound];
    want.sort();
    assert_eq!(
        changed_by(&mut w, vec![Op::Destroy { entity: key.into() }]),
        want
    );
}

#[test]
fn changed_since_resolves_handles_and_spans_patches_after_seq_only() {
    let (mut w, cellar, chest, key) = seeded();
    let start = w.last_seq().unwrap();
    let a = w
        .apply_patch(&Patch::new(vec![
            create("rope", "Rope", "item", None),
            mv("rope", Some(chest.into())),
        ]))
        .unwrap();
    let rope = a.created[0];
    let mid = w.last_seq().unwrap();
    rejected(
        &mut w,
        vec![Op::Destroy {
            entity: cellar.into(),
        }],
    );
    w.apply_patch(&Patch::new(vec![mv(key, Some(cellar.into()))]))
        .unwrap();
    let all: Vec<_> = w.changed_since(start).unwrap().into_iter().collect();
    let mut want = vec![rope, chest, key, cellar];
    want.sort();
    assert_eq!(all, want);
    let later: Vec<_> = w.changed_since(mid).unwrap().into_iter().collect();
    assert_eq!(later, vec![cellar, key]);
}

#[test]
fn a_rejected_patch_changes_nothing() {
    let (mut w, cellar, _, _) = seeded();
    let before = w.last_seq().unwrap();
    rejected(
        &mut w,
        vec![Op::Destroy {
            entity: cellar.into(),
        }],
    );
    assert!(w.last_seq().unwrap() > before);
    assert!(w.changed_since(before).unwrap().is_empty());
}

#[test]
fn other_events_change_nothing() {
    let (w, ..) = seeded();
    let before = w.last_seq().unwrap();
    w.log_event(EventKind::Input, &json!({"text": "look"}))
        .unwrap();
    assert!(w.changed_since(before).unwrap().is_empty());
}
