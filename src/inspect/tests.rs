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

fn with_sheet() -> Fixture {
    let mut f = fixture();
    let specs: Vec<stats::StatSpec> = serde_json::from_value(stats::sample_sheet()).unwrap();
    stats::create_sheet(&mut f.world, &specs, 7)
        .unwrap()
        .unwrap();
    f
}

/// The text between `Stat sheet:` and the engine counts.
fn sheet_part(out: &str) -> &str {
    out.split("\nEvents:").next().unwrap()
}

#[test]
fn stats_shows_the_stat_sheet_with_thresholds_capacity_and_growth() {
    let f = with_sheet();
    let ids: Vec<_> = stats::stat_rules(&f.world)
        .unwrap()
        .into_iter()
        .map(|r| (r.def.trigger.clone(), r.id))
        .collect();
    let id = |trigger: &str| ids.iter().find(|(t, _)| t == trigger).unwrap().1;
    let out = stats(&f.world).unwrap();
    let want = format!(
        "Stat sheet:\n  \
         brawn 2 [0..5]: strength and endurance\n    \
         #{} carry limit = 6 + 2 per point of brawn (now carry limit 10)\n    \
         grows #{} hard labour finished (+1)\n  \
         nerve 3 [0..5]: steadiness before the uncanny\n    \
         threshold #{} nerve falls to 0 or below => Your nerve breaks; you flee back the way you came, shaking.\n    \
         grows #{} facing a horror without fleeing (+1)",
        id("carry limit = 6 + 2 per point of brawn"),
        id("hard labour finished"),
        id("nerve falls to 0 or below"),
        id("facing a horror without fleeing"),
    );
    assert_eq!(sheet_part(&out), want, "\n{out}");
    assert!(
        out.contains("Rules: 6 (0 world active, 0 world retired, 6 stat)"),
        "{out}"
    );
}

#[test]
fn stats_marks_thresholds_already_reached_and_live_capacity() {
    let mut f = with_sheet();
    stats::adjust_stat(&mut f.world, "nerve", -3, "the thing in the well", None)
        .unwrap()
        .unwrap();
    stats::adjust_stat(&mut f.world, "brawn", -1, "a wrenched back", None)
        .unwrap()
        .unwrap();
    let out = stats(&f.world).unwrap();
    let sheet = sheet_part(&out);
    assert!(sheet.contains("nerve 0 [0..5]"), "{sheet}");
    assert!(
        sheet.contains("nerve falls to 0 or below (reached) =>"),
        "{sheet}"
    );
    assert!(sheet.contains("brawn 1 [0..5]"), "{sheet}");
    assert!(sheet.contains("(now carry limit 8)"), "{sheet}");
}

#[test]
fn stats_says_when_there_is_no_sheet() {
    let f = fixture();
    let out = stats(&f.world).unwrap();
    assert!(out.starts_with("Stat sheet: none yet\nEvents: "), "{out}");
}

/// The fixture plus a cellar, a bible naming three NPCs, and NPCs in the
/// cellar, in the hall (one inside a cart), and nowhere.
fn with_npcs() -> (Fixture, EntityId) {
    let mut f = fixture();
    let ops: Vec<Op> = serde_json::from_value(json!([
        {"op": "create", "handle": "bible", "name": "bible", "kind": "bible",
         "props": {"npcs": [
             {"name": "Old Hal", "description": "The miller."},
             {"name": "Brother Ash", "description": "Last monk."},
             "Wren"
         ]}},
        {"op": "create", "handle": "cellar", "name": "Cellar", "kind": "room"},
        {"op": "create", "handle": "hal", "name": "old hal", "kind": "miller", "location": "cellar",
         "props": {"mood": "wary", "acts": true}},
        {"op": "create", "handle": "cart", "name": "cart", "kind": "cart", "location": f.scope.room,
         "props": {"container": true}},
        {"op": "create", "handle": "rat", "name": "Rat", "kind": "rat", "location": "cart",
         "props": {"acts": true}},
        {"op": "create", "handle": "ghost", "name": "Ghost", "kind": "spirit", "props": {"acts": true}},
        {"op": "create", "name": "stuffed heron", "kind": "npc", "location": "cellar"}
    ]))
    .unwrap();
    let a = f.world.apply_engine_patch(&Patch::new(ops)).unwrap();
    let cellar = a.handles["cellar"];
    (f, cellar)
}

#[test]
fn npcs_lists_each_npc_with_where_it_is_and_its_props() {
    let (f, cellar) = with_npcs();
    let out = npcs(&f.world).unwrap();
    let hall = f.scope.room;
    let line = |name: &str| {
        out.lines()
            .find(|l| l.contains(name) && l.starts_with('#'))
            .unwrap_or_else(|| panic!("no line for {name}: {out}"))
            .to_owned()
    };
    // In another room, marked as a bible NPC despite the case difference.
    assert!(
        line("old hal").ends_with(&format!("old hal [bible]: in #{cellar} Cellar")),
        "{out}"
    );
    assert!(out.contains("\n  mood = \"wary\""), "{out}");
    // With the player, even inside a container.
    assert!(
        line("Rat").ends_with(&format!("Rat: with the player, in #{hall} Hall")),
        "{out}"
    );
    // Nowhere.
    assert!(line("Ghost").ends_with("Ghost: nowhere"), "{out}");
    // A kind of "npc" means nothing without acts: true.
    assert!(!out.contains("stuffed heron"), "{out}");
    // Bible NPCs with no entity yet; Old Hal is in the world.
    let tail = out
        .split("In the bible, not yet in the world:")
        .nth(1)
        .unwrap();
    assert_eq!(
        tail.trim().lines().map(str::trim).collect::<Vec<_>>(),
        ["Brother Ash", "Wren"]
    );
    // The engine's other entities aren't listed.
    assert!(!out.contains("crowbar") && !out.contains("you"), "{out}");
}

#[test]
fn npcs_says_when_there_are_none() {
    let f = fixture();
    assert_eq!(npcs(&f.world).unwrap(), "No NPCs yet.");
}

#[test]
fn npc_sightings_are_shared_with_the_tick() {
    let (f, cellar) = with_npcs();
    let s = crate::game::npcs(&f.world).unwrap();
    let names: Vec<_> = s
        .iter()
        .map(|s| (s.npc.name.as_str(), s.room, s.with_player))
        .collect();
    assert_eq!(
        names,
        [
            ("old hal", Some(cellar), false),
            ("Rat", Some(f.scope.room), true),
            ("Ghost", None, false)
        ]
    );
}

#[test]
fn relations_lists_both_directions_with_props() {
    let mut f = fixture();
    let ops: Vec<Op> = serde_json::from_value(json!([
        {"op": "relate", "subject": f.crowbar, "rel": "opens", "object": f.oak,
         "props": {"noisily": true}},
        {"op": "relate", "subject": f.iron, "rel": "faces", "object": f.crowbar},
    ]))
    .unwrap();
    f.world.apply_patch(&Patch::new(ops)).unwrap();
    let (bar, oak, iron) = (f.crowbar, f.oak, f.iron);
    assert_eq!(
        relations(&f.world, "crowbar").unwrap(),
        format!(
            "#{bar} crowbar (item)\n  -opens-> #{oak} oak door {{\"noisily\":true}}\n  \
             <-faces- #{iron} iron door"
        )
    );
    assert_eq!(
        relations(&f.world, &format!("#{}", f.scope.room)).unwrap(),
        format!("#{} Hall (room)\n  no relations", f.scope.room)
    );
    assert_eq!(
        relations(&f.world, "").unwrap(),
        "Usage: :relations <#id | name>"
    );
    assert!(
        relations(&f.world, "door")
            .unwrap()
            .contains("matches several")
    );
}

#[test]
fn why_shows_relations_and_their_cascades() {
    let mut f = fixture();
    let (bar, oak) = (f.crowbar, f.oak);
    let ops: Vec<Op> = serde_json::from_value(json!([
        {"op": "relate", "subject": bar, "rel": "opens", "object": oak},
    ]))
    .unwrap();
    f.world.apply_patch(&Patch::new(ops)).unwrap();
    let out = why(&f.world, "oak door").unwrap();
    assert!(
        out.contains(&format!("  <-opens- #{bar} crowbar\n")),
        "{out}"
    );
    assert!(
        out.contains(&format!("patch by director: relate #{bar} opens #{oak}")),
        "{out}"
    );

    f.world
        .apply_patch(&Patch::new(vec![Op::Destroy { entity: bar.into() }]))
        .unwrap();
    let out = why(&f.world, "oak door").unwrap();
    assert!(!out.contains("<-opens-"), "{out}");
    assert!(
        out.contains(&format!(
            "patch by director: cascade removed #{bar} opens #{oak}"
        )),
        "{out}"
    );
    let out = why(&f.world, &bar.to_string()).unwrap();
    assert!(
        out.contains(&format!(
            "destroy #{bar}; cascade removed #{bar} opens #{oak}"
        )),
        "{out}"
    );
}

#[test]
fn log_lists_recent_events_one_line_each() {
    let mut f = fixture();
    kick_turn(&mut f);
    let out = log(&f.world, "").unwrap();
    assert!(out.starts_with("#1 input: seed: a damp keep\n"), "{out}");
    assert!(out.contains("input: > kick the oak door"), "{out}");
    assert!(
        out.contains("rule_change: rule #") && out.contains("\"Kicking a rotten door\" minted v1"),
        "{out}"
    );
    assert!(
        out.contains("patch: by rule #") && out.contains(&format!("set #{}.broken = true", f.oak)),
        "{out}"
    );
    assert!(out.lines().all(|l| l.starts_with('#')), "{out}");

    let last = f.world.last_seq().unwrap();
    let two = log(&f.world, "2").unwrap();
    assert_eq!(two.lines().count(), 3, "{two}");
    assert!(
        two.starts_with(&format!("({} earlier event(s) not shown", last - 2)),
        "{two}"
    );
    assert!(
        two.lines()
            .last()
            .unwrap()
            .starts_with(&format!("#{last} ")),
        "{two}"
    );
    assert_eq!(log(&f.world, "all").unwrap().lines().count() as i64, last);
}

#[test]
fn log_filters_by_kind_and_caps_at_twenty() {
    let f = fixture();
    for i in 0..25 {
        f.world
            .log_event(
                EventKind::Roll,
                &json!({"stat": "craft", "die": i, "value": 2,
                "total": i + 2, "difficulty": 10, "result": "failure", "reason": "prying"}),
            )
            .unwrap();
    }
    let rolls = log(&f.world, "roll").unwrap();
    assert!(
        rolls.starts_with("(5 earlier event(s) not shown"),
        "{rolls}"
    );
    assert_eq!(rolls.lines().count(), 21, "{rolls}");
    assert!(
        rolls.ends_with("roll: craft d20 24 + 2 = 26 vs 10: failure (prying)"),
        "{rolls}"
    );
    let inputs = log(&f.world, "input patch").unwrap();
    assert_eq!(inputs.lines().count(), 2, "{inputs}");
    assert_eq!(log(&f.world, "tick").unwrap(), "No events logged.");
    assert!(
        log(&f.world, "bogus")
            .unwrap()
            .starts_with("Unknown :log word \"bogus\"")
    );
}

#[test]
fn log_shows_one_event_in_full() {
    let f = fixture();
    let seq = f
        .world
        .log_event(
            EventKind::Narration,
            &json!({"text": "Dust.\n\nMore dust."}),
        )
        .unwrap();
    let out = log(&f.world, &format!("#{seq}")).unwrap();
    let (head, rest) = out.split_once('\n').unwrap();
    assert!(head.starts_with(&format!("#{seq} narration at ")), "{out}");
    assert_eq!(rest, "Dust.\n\nMore dust.");

    let out = log(&f.world, "#2").unwrap();
    assert!(out.contains("by engine: create #"), "{out}");
    assert!(out.contains("\n{\n  \"by\": \"engine\""), "{out}");
    assert_eq!(log(&f.world, "#999").unwrap(), "No event #999.");
    assert!(log(&f.world, "#x").unwrap().starts_with("Usage: :log"));
}

#[test]
fn rejected_creates_show_no_created_id() {
    let mut f = fixture();
    let bad = Patch::new(vec![
        serde_json::from_value(json!({"op": "create", "name": "pebble", "kind": "item",
            "location": f.scope.room}))
        .unwrap(),
        Op::Destroy {
            entity: 9999.into(),
        },
    ]);
    assert!(f.world.apply_patch(&bad).is_err());
    let out = log(&f.world, "patch_rejected").unwrap();
    assert!(
        out.contains(&format!("create \"pebble\" (item) in #{}", f.scope.room)),
        "{out}"
    );
    let hall = why(&f.world, "Hall").unwrap();
    assert!(
        hall.contains(&format!("create \"pebble\" (item) in #{}", f.scope.room)),
        "{hall}"
    );
}

#[test]
fn time_shows_the_clock_and_recent_advances() {
    let mut world = World::open_in_memory().unwrap();
    world
        .apply_engine_patch(&Patch::new(vec![Op::Create {
            handle: None,
            name: "World bible".into(),
            kind: game::BIBLE_KIND.into(),
            location: None,
            props: Default::default(),
        }]))
        .unwrap();
    assert_eq!(
        time(&world).unwrap(),
        "It is day 1, 08:00 (morning).\nNo time has passed yet."
    );
    time::advance(&mut world, 10, time::Source::Default).unwrap();
    time::advance(&mut world, 90, time::Source::Director).unwrap();
    assert_eq!(
        time(&world).unwrap(),
        "It is day 1, 09:40 (morning).\nRecent turns:\n  \
         #5 +1 h 30 min (director) -> day 1, 09:40 (morning)\n  \
         #3 +10 min (default) -> day 1, 08:10 (morning)"
    );
    let log = log(&world, "time").unwrap();
    assert!(
        log.contains("+1 h 30 min (director) -> day 1, 09:40 (morning)"),
        "{log}"
    );
}
