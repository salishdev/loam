use serde_json::json;

use super::*;

fn patch(ops: serde_json::Value) -> Patch {
    Patch::new(serde_json::from_value(ops).unwrap())
}

#[test]
fn starts_with_the_terms_the_engine_reads() {
    let world = World::open_in_memory().unwrap();
    let index = index(&world).unwrap();
    for t in ["closed", "locked", "portable", "bulk", "slot"] {
        assert!(index.split(", ").any(|x| x == t), "{index}");
    }
    assert!(!index.contains('?'));
}

#[test]
fn new_props_and_enum_values_are_registered_once() {
    let mut world = World::open_in_memory().unwrap();
    let added = register_patch(
        &mut world,
        &patch(json!([
            {"op": "create", "name": "door", "kind": "door",
             "props": {"rotten": true, "_carrier": true, "bulk": 3, "colour": "moss green",
                       "description": "a long prose description of the door"}},
        ])),
    )
    .unwrap();
    assert_eq!(added, ["colour", "rotten"]);
    let t = terms(&world).unwrap();
    assert_eq!(t["rotten"].values.keys().collect::<Vec<_>>(), ["true"]);
    assert_eq!(
        t["colour"].values.keys().collect::<Vec<_>>(),
        ["moss green"]
    );
    assert!(
        t["description"].values.is_empty(),
        "free text isn't recorded"
    );
    assert!(!t.contains_key("_carrier"));
    assert!(index(&world).unwrap().contains("rotten?"));

    // A known term with a new value reports the value; repeats report nothing.
    let added = register_patch(
        &mut world,
        &patch(json!([
            {"op": "set_prop", "entity": 1, "key": "rotten", "value": false},
            {"op": "set_prop", "entity": 1, "key": "colour", "value": "moss green"},
            {"op": "set_prop", "entity": 1, "key": "colour", "value": "a very long sentence about colour"}
        ])),
    )
    .unwrap();
    assert_eq!(added, ["rotten=false"]);
    assert_eq!(world.entities_of_kind(GLOSSARY_KIND).unwrap().len(), 1);
}

#[test]
fn definitions_fill_in_meanings_and_survive_replay() {
    let mut world = World::open_in_memory().unwrap();
    register_patch(
        &mut world,
        &patch(json!([{"op": "create", "name": "x", "kind": "x", "props": {"rotten": true}}])),
    )
    .unwrap();
    let defs: Vec<Definition> = serde_json::from_value(json!([
        {"name": "rotten", "meaning": "wood soft with decay", "values": {"true": "will give way"}}
    ]))
    .unwrap();
    define(&mut world, &defs).unwrap().unwrap();
    let t = &terms(&world).unwrap()["rotten"];
    assert_eq!(t.meaning, "wood soft with decay");
    assert_eq!(t.values["true"], "will give way");
    assert!(!index(&world).unwrap().contains('?'));

    let bad: Vec<Definition> =
        serde_json::from_value(json!([{"name": "_carrier", "meaning": "x"}])).unwrap();
    assert!(define(&mut world, &bad).unwrap().is_err());

    let replayed = world.replay_in_memory().unwrap();
    assert_eq!(terms(&replayed).unwrap(), terms(&world).unwrap());
}

#[test]
fn terms_are_engine_owned() {
    let mut world = World::open_in_memory().unwrap();
    register(&mut world, [("rotten", None)]).unwrap();
    let id = glossary_id(&world).unwrap().unwrap();
    // Terms live in one engine-owned component, so the director can't edit
    // them with a patch on the glossary entity.
    let props = world.entity(id).unwrap().unwrap().props;
    assert_eq!(props.keys().collect::<Vec<_>>(), vec![Terms::KEY]);
    let err = world
        .apply_patch(&patch(json!([
            {"op": "set_prop", "entity": id, "key": Terms::KEY, "value": {}}
        ])))
        .unwrap_err();
    assert!(err.to_string().contains("engine-owned"), "{err}");
    assert!(terms(&world).unwrap().contains_key("rotten"));
}
