use super::*;
use crate::llm::{ToolOutput, ToolUse};
use crate::world::{EntityRef, EventKind};
use serde_json::{Map, json};

fn create(handle: &str, kind: &str, location: Option<&str>, props: Value) -> Op {
    Op::Create {
        handle: Some(handle.into()),
        name: handle.into(),
        kind: kind.into(),
        location: location.map(EntityRef::from),
        props: props.as_object().cloned().unwrap_or_else(Map::new),
    }
}

/// Fixture entity ids by handle.
struct Ids(std::collections::BTreeMap<String, EntityId>);

impl Ids {
    fn id(&self, h: &str) -> EntityId {
        self.0[h]
    }
    fn loc(&self, w: &World, h: &str) -> Option<EntityId> {
        w.entity(self.id(h)).unwrap().unwrap().location
    }
    fn props(&self, w: &World, h: &str) -> Map<String, Value> {
        w.entity(self.id(h)).unwrap().unwrap().props
    }
}

/// hall: player, npc, sword, helm, ring, open chest (gem), locked box
/// (coin), statue (not portable), anvil (bulk 20). The player carries a bag
/// holding an apple. yard (elsewhere): pebble.
fn fixture() -> (World, Ids) {
    let mut w = World::open_in_memory().unwrap();
    let a = w
        .apply_patch(&Patch::new(vec![
            create("hall", "room", None, json!({})),
            create("yard", "room", None, json!({})),
            create("player", "person", Some("hall"), json!({})),
            create("npc", "person", Some("hall"), json!({})),
            create(
                "sword",
                "item",
                Some("hall"),
                json!({"slot": ["main_hand", "off_hand"]}),
            ),
            create("helm", "item", Some("hall"), json!({"slot": "head"})),
            create("cap", "item", Some("hall"), json!({"slot": "head"})),
            create("ring", "item", Some("hall"), json!({})),
            create("chest", "container", Some("hall"), json!({"bulk": 5})),
            create("gem", "item", Some("chest"), json!({})),
            create("box", "container", Some("hall"), json!({"locked": true})),
            create("coin", "item", Some("box"), json!({})),
            create("statue", "item", Some("hall"), json!({"portable": false})),
            create("anvil", "item", Some("hall"), json!({"bulk": 20})),
            create("bag", "container", Some("hall"), json!({})),
            create("apple", "item", Some("bag"), json!({})),
            create("pebble", "item", Some("yard"), json!({})),
        ]))
        .unwrap();
    let f = Ids(a.handles);
    make_carrier(&mut w, f.id("player")).unwrap();
    make_carrier(&mut w, f.id("npc")).unwrap();
    Inventory::default()
        .take(&mut w, f.id("player"), f.id("bag"))
        .unwrap();
    (w, f)
}

fn refused<T: std::fmt::Debug>(r: Result<T>) -> String {
    match r {
        Err(InventoryError::Refused(reason)) => reason,
        other => panic!("expected refusal, got {other:?}"),
    }
}

// ---- reachability ----

#[test]
fn take_item_in_same_room() {
    let (mut w, f) = fixture();
    let inv = Inventory::default();
    inv.take(&mut w, f.id("player"), f.id("sword")).unwrap();
    assert_eq!(f.loc(&w, "sword"), Some(f.id("player")));
}

#[test]
fn take_from_open_container_in_room() {
    let (mut w, f) = fixture();
    Inventory::default()
        .take(&mut w, f.id("player"), f.id("gem"))
        .unwrap();
    assert_eq!(f.loc(&w, "gem"), Some(f.id("player")));
}

#[test]
fn closed_or_locked_container_blocks_reach() {
    let (mut w, f) = fixture();
    let inv = Inventory::default();
    let why = refused(inv.take(&mut w, f.id("player"), f.id("coin")));
    assert!(
        why.contains("inside box") && why.contains("closed"),
        "{why}"
    );

    let chest = f.id("chest");
    w.apply_patch(&Patch::new(vec![Op::SetProp {
        entity: chest.into(),
        key: "closed".into(),
        value: json!(true),
    }]))
    .unwrap();
    refused(inv.take(&mut w, f.id("player"), f.id("gem")));
}

#[test]
fn items_elsewhere_are_out_of_reach() {
    let (mut w, f) = fixture();
    let why = refused(Inventory::default().take(&mut w, f.id("player"), f.id("pebble")));
    assert!(why.contains("not within reach"), "{why}");
}

#[test]
fn items_held_by_another_carrier_are_out_of_reach() {
    let (mut w, f) = fixture();
    let inv = Inventory::default();
    inv.take(&mut w, f.id("npc"), f.id("ring")).unwrap();
    let why = refused(inv.take(&mut w, f.id("player"), f.id("ring")));
    assert!(why.contains("held by npc"), "{why}");
}

#[test]
fn non_portable_items_carriers_and_rooms_cannot_be_taken() {
    let (mut w, f) = fixture();
    let inv = Inventory::default();
    let p = f.id("player");
    assert!(refused(inv.take(&mut w, p, f.id("statue"))).contains("can't be picked up"));
    assert!(refused(inv.take(&mut w, p, f.id("npc"))).contains("can't be carried"));
    refused(inv.take(&mut w, p, f.id("hall")));
    refused(inv.take(&mut w, p, p));
}

#[test]
fn take_already_carried_or_from_own_bag() {
    let (mut w, f) = fixture();
    let inv = Inventory::default();
    let p = f.id("player");
    assert!(refused(inv.take(&mut w, p, f.id("bag"))).contains("already carrying"));
    // Pulling the apple out of the bag keeps it carried, in hand.
    inv.take(&mut w, p, f.id("apple")).unwrap();
    assert_eq!(f.loc(&w, "apple"), Some(p));
}

#[test]
fn only_carriers_can_act() {
    let (mut w, f) = fixture();
    let why = refused(Inventory::default().take(&mut w, f.id("chest"), f.id("sword")));
    assert!(why.contains("can't carry things"), "{why}");
}

#[test]
fn drop_puts_item_in_carriers_location() {
    let (mut w, f) = fixture();
    let inv = Inventory::default();
    let p = f.id("player");
    inv.drop(&mut w, p, f.id("apple")).unwrap(); // nested in the bag
    assert_eq!(f.loc(&w, "apple"), Some(f.id("hall")));
    assert!(refused(inv.drop(&mut w, p, f.id("sword"))).contains("isn't carrying"));
}

#[test]
fn closed_carried_bag_blocks_dropping_its_contents() {
    let (mut w, f) = fixture();
    let bag = f.id("bag");
    w.apply_patch(&Patch::new(vec![Op::SetProp {
        entity: bag.into(),
        key: "closed".into(),
        value: json!(true),
    }]))
    .unwrap();
    let why = refused(Inventory::default().drop(&mut w, f.id("player"), f.id("apple")));
    assert!(why.contains("closed"), "{why}");
}

#[test]
fn give_to_carrier_in_same_room() {
    let (mut w, f) = fixture();
    let inv = Inventory::default();
    let (p, n) = (f.id("player"), f.id("npc"));
    inv.give(&mut w, p, n, f.id("bag")).unwrap();
    assert_eq!(f.loc(&w, "bag"), Some(n));
    assert!(refused(inv.give(&mut w, p, f.id("chest"), f.id("bag"))).contains("can't hold things"));
    assert!(refused(inv.give(&mut w, p, n, f.id("bag"))).contains("isn't carrying"));

    // An npc in another room can't receive.
    inv.give(&mut w, n, p, f.id("bag")).unwrap();
    let yard = f.id("yard");
    w.apply_patch(&Patch::new(vec![Op::Move {
        entity: n.into(),
        to: Some(yard.into()),
    }]))
    .unwrap();
    assert!(refused(inv.give(&mut w, p, n, f.id("bag"))).contains("not within reach"));
}

#[test]
fn transfer_between_containers() {
    let (mut w, f) = fixture();
    let inv = Inventory::default();
    let p = f.id("player");
    // From the bag into the chest in the room.
    inv.transfer(&mut w, p, f.id("apple"), f.id("chest"))
        .unwrap();
    assert_eq!(f.loc(&w, "apple"), Some(f.id("chest")));
    // From the chest into the carried bag.
    inv.transfer(&mut w, p, f.id("gem"), f.id("bag")).unwrap();
    assert_eq!(f.loc(&w, "gem"), Some(f.id("bag")));
    // Not into a locked box, another carrier, itself or out of reach.
    assert!(refused(inv.transfer(&mut w, p, f.id("gem"), f.id("box"))).contains("closed"));
    assert!(refused(inv.transfer(&mut w, p, f.id("gem"), f.id("npc"))).contains("use give"));
    refused(inv.transfer(&mut w, p, f.id("bag"), f.id("bag")));
    refused(inv.transfer(&mut w, p, f.id("pebble"), f.id("bag")));
    // A bag can't go inside something it contains (store cycle check).
    inv.transfer(&mut w, p, f.id("chest"), f.id("bag")).unwrap();
    let why = refused(inv.transfer(&mut w, p, f.id("bag"), f.id("chest")));
    assert!(why.contains("cycle"), "{why}");
}

// ---- slots ----

#[test]
fn equip_uses_the_items_only_slot() {
    let (mut w, f) = fixture();
    let inv = Inventory::default();
    let p = f.id("player");
    inv.take(&mut w, p, f.id("helm")).unwrap();
    inv.equip(&mut w, p, f.id("helm"), None).unwrap();
    assert_eq!(f.props(&w, "helm")[EQUIPPED_PROP], json!("head"));
    let listing = inv.list(&w, p).unwrap();
    let helm = listing.items.iter().find(|i| i.id == f.id("helm")).unwrap();
    assert_eq!(helm.equipped.as_deref(), Some("head"));
}

#[test]
fn occupied_slot_is_a_conflict() {
    let (mut w, f) = fixture();
    let inv = Inventory::default();
    let p = f.id("player");
    for h in ["helm", "cap"] {
        inv.take(&mut w, p, f.id(h)).unwrap();
    }
    inv.equip(&mut w, p, f.id("helm"), None).unwrap();
    let why = refused(inv.equip(&mut w, p, f.id("cap"), Some("head")));
    assert!(why.contains("taken by helm"), "{why}");
    let why = refused(inv.equip(&mut w, p, f.id("helm"), None));
    assert!(why.contains("already equipped"), "{why}");
    // Free the slot and try again.
    inv.unequip(&mut w, p, Unequip::Slot("head".into()))
        .unwrap();
    assert!(!f.props(&w, "helm").contains_key(EQUIPPED_PROP));
    assert_eq!(f.loc(&w, "helm"), Some(p));
    inv.equip(&mut w, p, f.id("cap"), None).unwrap();
}

#[test]
fn multi_slot_item_takes_first_free_slot() {
    let (mut w, f) = fixture();
    let inv = Inventory::default();
    let p = f.id("player");
    inv.take(&mut w, p, f.id("ring")).unwrap();
    inv.equip(&mut w, p, f.id("ring"), Some("main_hand"))
        .unwrap();
    inv.take(&mut w, p, f.id("sword")).unwrap();
    inv.equip(&mut w, p, f.id("sword"), None).unwrap();
    assert_eq!(f.props(&w, "sword")[EQUIPPED_PROP], json!("off_hand"));
}

#[test]
fn slot_must_exist_and_fit() {
    let (mut w, f) = fixture();
    let inv = Inventory::default();
    let p = f.id("player");
    for h in ["helm", "ring"] {
        inv.take(&mut w, p, f.id(h)).unwrap();
    }
    assert!(refused(inv.equip(&mut w, p, f.id("helm"), Some("feet"))).contains("doesn't fit"));
    assert!(refused(inv.equip(&mut w, p, f.id("ring"), Some("tail"))).contains("no 'tail' slot"));
    assert!(refused(inv.equip(&mut w, p, f.id("ring"), None)).contains("say which slot"));
    // Only carried items can be equipped.
    assert!(refused(inv.equip(&mut w, p, f.id("cap"), None)).contains("isn't carrying"));
}

#[test]
fn equip_from_bag_moves_item_to_hand() {
    let (mut w, f) = fixture();
    let inv = Inventory::default();
    let p = f.id("player");
    inv.equip(&mut w, p, f.id("apple"), Some("main_hand"))
        .unwrap();
    assert_eq!(f.loc(&w, "apple"), Some(p));
}

#[test]
fn moving_an_equipped_item_unequips_it() {
    let (mut w, f) = fixture();
    let inv = Inventory::default();
    let p = f.id("player");
    inv.take(&mut w, p, f.id("helm")).unwrap();
    inv.equip(&mut w, p, f.id("helm"), None).unwrap();
    inv.drop(&mut w, p, f.id("helm")).unwrap();
    assert!(!f.props(&w, "helm").contains_key(EQUIPPED_PROP));
    let why = refused(inv.unequip(&mut w, p, Unequip::Item(f.id("helm"))));
    assert!(why.contains("doesn't have"), "{why}");
    let why = refused(inv.unequip(&mut w, p, Unequip::Slot("head".into())));
    assert!(why.contains("nothing in the 'head' slot"), "{why}");
}

#[test]
fn configured_slots_replace_defaults() {
    let (mut w, f) = fixture();
    let inv = Inventory::new(["tail"], FixedLimit(10.0));
    let p = f.id("player");
    inv.take(&mut w, p, f.id("ring")).unwrap();
    inv.equip(&mut w, p, f.id("ring"), Some("tail")).unwrap();
    inv.take(&mut w, p, f.id("helm")).unwrap();
    assert!(refused(inv.equip(&mut w, p, f.id("helm"), None)).contains("carriers don't have"));
}

// ---- capacity ----

#[test]
fn capacity_counts_bulk_including_bag_contents() {
    let (w, f) = fixture();
    let listing = Inventory::default().list(&w, f.id("player")).unwrap();
    assert_eq!(listing.load, 2.0); // bag + apple
    assert_eq!(listing.limit, DEFAULT_CARRY_LIMIT);
    let apple = listing
        .items
        .iter()
        .find(|i| i.id == f.id("apple"))
        .unwrap();
    assert_eq!(apple.inside, Some(f.id("bag")));
}

#[test]
fn taking_over_the_limit_is_refused() {
    let (mut w, f) = fixture();
    let inv = Inventory::default();
    let p = f.id("player");
    let why = refused(inv.take(&mut w, p, f.id("anvil")));
    assert!(why.contains("can't carry anvil"), "{why}");
    assert_eq!(f.loc(&w, "anvil"), Some(f.id("hall")));
    // A container counts with its contents: chest 5 + gem 1 fits (2 + 6 = 8).
    inv.take(&mut w, p, f.id("chest")).unwrap();
    assert_eq!(inv.list(&w, p).unwrap().load, 8.0);
    // 8 + 1 + 1 = 10 is at the limit, which is allowed; one more is not.
    inv.take(&mut w, p, f.id("sword")).unwrap();
    inv.take(&mut w, p, f.id("helm")).unwrap();
    refused(inv.take(&mut w, p, f.id("cap")));
    refused(inv.transfer(&mut w, p, f.id("cap"), f.id("bag")));
}

#[test]
fn recipient_limit_applies_to_give() {
    let (mut w, f) = fixture();
    let inv = Inventory::new(DEFAULT_SLOTS, |_: &World, c: EntityId| {
        Ok(if c == 4 { 1.0 } else { 10.0 }) // npc is #4
    });
    assert_eq!(f.id("npc"), 4);
    let why = refused(inv.give(&mut w, f.id("player"), f.id("npc"), f.id("bag")));
    assert!(why.contains("npc"), "{why}");
}

#[test]
fn limit_is_soft_when_it_drops() {
    let (mut w, f) = fixture();
    let p = f.id("player");
    let weak = Inventory::new(DEFAULT_SLOTS, FixedLimit(1.0));
    let listing = weak.list(&w, p).unwrap();
    assert!(listing.over_limit);
    // Over the limit: can't take more, can still drop.
    refused(weak.take(&mut w, p, f.id("ring")));
    weak.drop(&mut w, p, f.id("apple")).unwrap();
    assert!(!weak.list(&w, p).unwrap().over_limit);
}

// ---- director guard ----

#[test]
fn director_patches_cannot_change_inventory() {
    let (mut w, f) = fixture();
    let (p, apple, sword) = (f.id("player"), f.id("apple"), f.id("sword"));
    let bag = f.id("bag");
    let hall = f.id("hall");
    let attempts = vec![
        // Out of the inventory (even from a carried bag).
        Op::Move {
            entity: apple.into(),
            to: Some(hall.into()),
        },
        // Into the inventory, directly or into a carried bag.
        Op::Move {
            entity: sword.into(),
            to: Some(p.into()),
        },
        Op::Move {
            entity: sword.into(),
            to: Some(bag.into()),
        },
        // Creating straight into it.
        create("loot", "item", None, json!({})),
        // Engine-owned props.
        Op::SetProp {
            entity: sword.into(),
            key: EQUIPPED_PROP.into(),
            value: json!("head"),
        },
        Op::RemoveProp {
            entity: p.into(),
            key: CARRIER_PROP.into(),
        },
        create("x", "item", None, json!({"_carrier": true})),
    ];
    for (i, mut op) in attempts.into_iter().enumerate() {
        if let Op::Create {
            handle, location, ..
        } = &mut op
            && handle.as_deref() == Some("loot")
        {
            *location = Some(bag.into());
        }
        match w.apply_patch(&Patch::new(vec![op])) {
            Err(crate::world::ApplyError::Rejected(r)) => {
                assert!(
                    r.reason.contains("inventory") || r.reason.contains("engine-owned"),
                    "attempt {i}: {}",
                    r.reason
                );
            }
            other => panic!("attempt {i} should be rejected, got {other:?}"),
        }
    }
    assert_eq!(f.loc(&w, "apple"), Some(bag));
}

#[test]
fn director_can_still_move_carriers_and_edit_carried_items() {
    let (mut w, f) = fixture();
    let (p, apple, yard) = (f.id("player"), f.id("apple"), f.id("yard"));
    w.apply_patch(&Patch::new(vec![
        Op::Move {
            entity: p.into(),
            to: Some(yard.into()),
        },
        Op::SetProp {
            entity: apple.into(),
            key: "rotten".into(),
            value: json!(true),
        },
        Op::Destroy {
            entity: apple.into(),
        },
    ]))
    .unwrap();
    assert_eq!(f.loc(&w, "player"), Some(yard));
    assert_eq!(f.loc(&w, "bag"), Some(p));
}

#[test]
fn inventory_changes_are_logged_as_engine_patches() {
    let (mut w, f) = fixture();
    let seq = Inventory::default()
        .take(&mut w, f.id("player"), f.id("sword"))
        .unwrap();
    let ev = w
        .events()
        .unwrap()
        .into_iter()
        .find(|e| e.seq == seq)
        .unwrap();
    assert_eq!(ev.kind, EventKind::Patch);
    assert_eq!(ev.payload["by"], json!("engine"));
    // Director patches don't carry the marker.
    let first = &w.events().unwrap()[0];
    assert!(first.payload.get("by").is_none());
}

// ---- tools ----

fn call(name: &str, input: Value) -> ToolUse {
    ToolUse {
        id: "toolu_1".into(),
        name: name.into(),
        input,
    }
}

#[test]
fn tool_definitions_cover_every_tool() {
    let names: Vec<_> = tool_definitions().into_iter().map(|t| t.name).collect();
    assert_eq!(
        names,
        [
            "inventory_list",
            "take",
            "drop",
            "give",
            "equip",
            "unequip",
            "transfer"
        ]
    );
}

#[test]
fn tools_act_for_the_player_by_default() {
    let (mut w, f) = fixture();
    let inv = Inventory::default();
    let p = f.id("player");
    let out = inv.handle_tool(&mut w, p, &call("take", json!({"item": f.id("helm")})));
    let Some(ToolOutput::Ok(body)) = out else {
        panic!("{out:?}")
    };
    let body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["load"], json!(3.0));
    assert_eq!(f.loc(&w, "helm"), Some(p));

    inv.handle_tool(&mut w, p, &call("equip", json!({"item": f.id("helm")})));
    inv.handle_tool(&mut w, p, &call("unequip", json!({"slot": "head"})));
    inv.handle_tool(
        &mut w,
        p,
        &call("give", json!({"item": f.id("helm"), "to": f.id("npc")})),
    );
    assert_eq!(f.loc(&w, "helm"), Some(f.id("npc")));
    // The npc acts through `carrier`.
    let npc = f.id("npc");
    inv.handle_tool(
        &mut w,
        p,
        &call("drop", json!({"item": f.id("helm"), "carrier": npc})),
    );
    assert_eq!(f.loc(&w, "helm"), Some(f.id("hall")));
    inv.handle_tool(
        &mut w,
        p,
        &call("transfer", json!({"item": f.id("gem"), "to": f.id("bag")})),
    );
    assert_eq!(f.loc(&w, "gem"), Some(f.id("bag")));

    let Some(ToolOutput::Ok(list)) = inv.handle_tool(&mut w, p, &call("inventory_list", json!({})))
    else {
        panic!()
    };
    let list: Value = serde_json::from_str(&list).unwrap();
    assert_eq!(list["items"].as_array().unwrap().len(), 3);
}

#[test]
fn impossible_moves_return_reasons() {
    let (mut w, f) = fixture();
    let inv = Inventory::default();
    let p = f.id("player");
    let out = inv.handle_tool(&mut w, p, &call("take", json!({"item": f.id("statue")})));
    assert_eq!(
        out,
        Some(ToolOutput::Err(format!(
            "statue (#{}) can't be picked up",
            f.id("statue")
        )))
    );
    for (name, input) in [
        ("take", json!({})),
        ("take", json!({"item": "sword"})),
        ("unequip", json!({})),
        ("drop", json!({"item": 999})),
    ] {
        match inv.handle_tool(&mut w, p, &call(name, input.clone())) {
            Some(ToolOutput::Err(_)) => {}
            other => panic!("{name} {input}: {other:?}"),
        }
    }
}

#[test]
fn other_tools_are_not_handled() {
    let (mut w, f) = fixture();
    let p = f.id("player");
    assert_eq!(
        Inventory::default().handle_tool(&mut w, p, &call("query_entity", json!({}))),
        None
    );
}

#[test]
fn inventory_session_replays_identically() {
    let (mut w, f) = fixture();
    let inv = Inventory::default();
    let p = f.id("player");
    inv.take(&mut w, p, f.id("helm")).unwrap();
    inv.equip(&mut w, p, f.id("helm"), None).unwrap();
    inv.give(&mut w, p, f.id("npc"), f.id("bag")).unwrap();
    refused(inv.take(&mut w, p, f.id("statue")));
    let replayed = w.replay_in_memory().unwrap();
    for h in ["helm", "bag", "apple", "player", "npc"] {
        assert_eq!(
            replayed.entity(f.id(h)).unwrap(),
            w.entity(f.id(h)).unwrap(),
            "{h}"
        );
    }
}
