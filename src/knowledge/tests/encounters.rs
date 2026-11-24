//! People and notable items remembered as seen, explicit links from a
//! mentioned entry to the entity it turned out to be, and later
//! corrections.

use serde_json::Value;

use super::*;
use crate::llm::ContentBlock;

/// A word only the hidden props carry: no report may ever show it.
const SECRET: &str = "abbotkiller";

fn learn_call(input: Value) -> Response {
    Response::canned_tool_use("l", "learn", input)
}

/// Play a director turn: each tool call in turn, then `narration`.
fn turn(game: &mut Game, client: &ScriptedClient, calls: &[Response], narration: &str) {
    for c in calls {
        client.push(c.clone());
    }
    client.push(Response::canned_text(narration));
    game.turn(client, "look around", &mut |_| {}).unwrap();
}

/// Apply a director patch outside any turn (the world changing without
/// the player there to see it).
fn patch(game: &mut Game, ops: Value) -> crate::world::Applied {
    game.world_mut()
        .apply_patch(&Patch::new(serde_json::from_value(ops).unwrap()))
        .unwrap()
}

/// An NPC named `name` in `room`, carrying a hidden prop.
fn npc(game: &mut Game, name: &str, room: EntityId) -> EntityId {
    patch(
        game,
        json!([{"op": "create", "handle": "n", "name": name, "kind": "person", "location": room,
                "props": {"acts": true, "secret": SECRET, "mood": "wary"}}]),
    )
    .handles["n"]
}

fn people(world: &World) -> Vec<Entry> {
    entries(world)
        .unwrap()
        .into_iter()
        .filter(|e| e.category == Category::Person)
        .collect()
}

fn narration_seq(world: &World) -> i64 {
    world.last_event(EventKind::Narration).unwrap().unwrap().seq
}

/// Every Player-mode report about the compendium, concatenated.
fn every_report(world: &World) -> String {
    let mut out = compendium(world, "");
    for c in Category::ALL {
        out.push_str(&compendium(world, c.heading()));
    }
    for e in entries(world).unwrap() {
        out.push_str(&compendium(world, &format!("#{}", e.id)));
    }
    let search = compendium(world, &format!("search {SECRET}"));
    assert!(search.starts_with("Nothing you know mentions"), "{search}");
    out
}

const RUMOUR: &str = "\"Old Hal drowned last spring,\" Brother Ash says, \"or so I heard.\"";

/// A begun game where Brother Ash has told the player Old Hal drowned:
/// a person known only by mention. Returns the entry's id.
fn hal_mentioned() -> (Game, ScriptedClient, EntityId) {
    let (mut game, client) = begun();
    turn(
        &mut game,
        &client,
        &[learn_call(
            json!({"name": "Old Hal", "category": "person", "facts": [
                {"text": "Old Hal drowned last spring.", "excerpt": "Old Hal drowned last spring",
                 "claimed_by": "Brother Ash"}
            ]}),
        )],
        RUMOUR,
    );
    let hal = place(game.world(), "Old Hal");
    assert!(!hal.linked);
    (game, client, hal.id)
}

#[test]
fn a_mentioned_person_is_linked_when_met_keeping_its_id_and_claims() {
    let (mut game, client, entry) = hal_mentioned();
    let chapel = player_room(&game);
    let hal = npc(&mut game, "Old Hal", chapel);

    // Hal turns up, but nobody says he is that Hal: a shared name alone
    // neither merges nor duplicates the entry.
    turn(
        &mut game,
        &client,
        &[],
        "Old Hal shuffles in, nets over one shoulder.",
    );
    let known = people(game.world());
    assert_eq!(known.len(), 1, "{known:?}");
    assert_eq!(known[0].encounters, 0);
    assert!(!known[0].linked);

    // The director recognises him: a same-name new entry is refused with
    // the known one's id, and the explicit link (no new facts) is staged.
    turn(
        &mut game,
        &client,
        &[
            learn_call(
                json!({"name": "Old Hal", "category": "person", "subject": hal, "facts": []}),
            ),
            learn_call(json!({"entry": entry, "subject": hal})),
        ],
        "Old Hal looks up from his nets and grins.",
    );
    let requests = client.requests();
    let refusal = requests[requests.len() - 2]
        .messages
        .last()
        .unwrap()
        .content[0]
        .clone();
    match refusal {
        ContentBlock::ToolResult {
            content, is_error, ..
        } => {
            assert!(is_error);
            assert!(content.contains(&format!("#{entry}")), "{content}");
        }
        other => panic!("expected a tool result, got {other:?}"),
    }
    let world = game.world();
    let met = narration_seq(world);
    let known = people(world);
    assert_eq!(known.len(), 1, "{known:?}");
    let e = &known[0];
    assert_eq!(e.id, entry, "the entry keeps its identity");
    assert!(e.linked);
    assert_eq!((e.encounters, e.first_seen, e.last_seen), (1, met, met));
    assert_eq!(e.facts[0].claimed_by.as_deref(), Some("Brother Ash"));
    let detail = compendium(world, "Old Hal");
    assert!(
        detail.contains("1. Brother Ash claims: Old Hal drowned last spring."),
        "{detail}"
    );
    assert!(
        detail.contains(&format!("Last seen at Drowned Chapel [#{met}].")),
        "{detail}"
    );

    // A second, different Old Hal is met: his own entry, not a merge.
    let other = npc(&mut game, "Old Hal", chapel);
    turn(
        &mut game,
        &client,
        &[],
        "Another Old Hal, younger, steps from behind the font.",
    );
    let known = people(game.world());
    assert_eq!(known.len(), 2, "{known:?}");
    assert_eq!(known[0].id, entry);
    assert_eq!(
        known[0].encounters, 2,
        "the first Hal is still here and named"
    );
    assert_ne!(known[1].id, entry);
    assert!(known[1].facts.is_empty(), "the newcomer inherits no claims");
    let _ = other;
}

#[test]
fn a_later_observation_corrects_a_claim_keeping_both_sources() {
    let (mut game, client, entry) = hal_mentioned();
    let told = narration_seq(game.world());
    let chapel = player_room(&game);
    let hal = npc(&mut game, "Old Hal", chapel);

    // An out-of-range correction is refused.
    turn(
        &mut game,
        &client,
        &[learn_call(json!({"entry": entry, "facts": [
            {"text": "x", "excerpt": "Old Hal is alive", "supersedes": [2]}
        ]}))],
        "Old Hal is alive.",
    );
    assert_eq!(place(game.world(), "Old Hal").facts.len(), 1);

    // The director looks up the fact numbers, then corrects the rumour.
    client.push(Response::canned_tool_use(
        "r",
        "recall",
        json!({"query": "Old Hal"}),
    ));
    turn(
        &mut game,
        &client,
        &[learn_call(
            json!({"entry": entry, "subject": hal, "facts": [
                {"text": "Old Hal is alive: he did not drown.", "excerpt": "very much alive, and dry",
                 "supersedes": [1]}
            ]}),
        )],
        "Old Hal stands before you, very much alive, and dry as tinder.",
    );
    let requests = client.requests();
    let recalled = requests
        .iter()
        .rev()
        .find_map(|r| match r.messages.last()?.content.first()? {
            ContentBlock::ToolResult { content, .. } if content.contains("not linked") => {
                Some(content.clone())
            }
            _ => None,
        })
        .expect("recall answered");
    assert!(
        recalled.contains(&format!("#{entry} Old Hal (person)"))
            && recalled.contains("1. (claimed by Brother Ash) Old Hal drowned last spring."),
        "{recalled}"
    );

    let world = game.world();
    let seen = narration_seq(world);
    let e = place(world, "Old Hal");
    assert_eq!(e.id, entry);
    assert_eq!(e.facts.len(), 2);
    // The claim stays as it was: attributed, with its own source.
    assert_eq!(e.facts[0].kind, FactKind::Claimed);
    assert_eq!(e.facts[0].claimed_by.as_deref(), Some("Brother Ash"));
    assert_eq!(e.facts[0].sources[0].event, told);
    assert_eq!(e.facts[1].kind, FactKind::Observed);
    assert_eq!(e.facts[1].sources[0].event, seen);
    assert_eq!(e.facts[1].supersedes, [1]);
    assert_eq!(e.superseded(), [1]);
    // The displayed account follows the correction.
    assert_eq!(e.summary, "Old Hal is alive: he did not drown.");
    let detail = compendium(world, "Old Hal");
    assert!(
        detail.contains(&format!(
            "1. Brother Ash claims: Old Hal drowned last spring. (superseded by 2) [#{told}"
        )),
        "{detail}"
    );
    assert!(
        detail.contains(&format!(
            "2. Old Hal is alive: he did not drown. (corrects 1) [#{seen}"
        )),
        "{detail}"
    );
    assert!(detail.starts_with(&format!(
        "Old Hal · person · entry #{entry}\nOld Hal is alive: he did not drown.\n"
    )));
}

#[test]
fn the_opening_scene_remembers_the_people_it_names_and_nothing_hidden() {
    let (mut game, client) = crate::game::tests::new_game();
    client.push(Response::canned_tool_use(
        "r1",
        "create_room",
        json!({"name": "Drowned Chapel", "region": "Saltmarsh",
               "description": "Brackish water laps at the pews.", "exits": {},
               "contents": [
                   {"name": "Brother Ash", "kind": "person", "props": {"acts": true, "secret": SECRET}},
                   {"name": "lurker", "kind": "person", "props": {"acts": true, "secret": SECRET}},
                   {"handle": "chest", "name": "chest", "kind": "feature", "props": {"container": true}},
                   {"name": "Saint's Finger", "kind": "item", "inside": "chest", "props": {"secret": SECRET}}
               ]}),
    ));
    client.push(Response::canned_text(
        "Brother Ash kneels by a chest in the flooded chapel.",
    ));
    let before = client.requests().len();
    game.begin(&client, &mut |_| {}).unwrap();
    assert_eq!(client.requests().len() - before, 2, "no extra model pass");
    let world = game.world();
    let opening = narration_seq(world);
    assert_eq!(names(world), ["Drowned Chapel", "Brother Ash"]);
    let ash = place(world, "Brother Ash");
    assert_eq!((ash.encounters, ash.first_seen), (1, opening));
    assert!(ash.facts.is_empty() && ash.summary.is_empty());
    assert_eq!(
        ash.seen_at,
        Some(place(world, "Drowned Chapel").id),
        "seen where the player is"
    );
    let all = every_report(world);
    assert!(!all.contains(SECRET), "{all}");
    assert!(!all.contains("lurker") && !all.contains("Saint"), "{all}");
    assert!(all.contains("Last seen at Drowned Chapel"), "{all}");
}

fn interpret(matches: Value) -> Response {
    Response::canned_tool_use("i", "interpret", json!({"matches": matches, "clean": true}))
}

#[test]
fn fast_path_turns_remember_known_people_and_notable_items_without_another_call() {
    let (mut game, client) = begun();
    let chapel = player_room(&game);
    let made = patch(
        &mut game,
        json!([
            {"op": "create", "handle": "oak", "name": "oak door", "kind": "door", "location": chapel, "props": {"rotten": true}},
            {"op": "create", "handle": "elm", "name": "elm door", "kind": "door", "location": chapel, "props": {"rotten": true}},
            {"op": "create", "handle": "hal", "name": "Old Hal", "kind": "person", "location": chapel,
             "props": {"acts": true, "secret": SECRET}},
            {"op": "create", "handle": "key", "name": "iron key", "kind": "item", "location": chapel,
             "props": {"opens": "the crypt", "secret": SECRET}}
        ]),
    );
    let (oak, elm, key) = (
        made.handles["oak"],
        made.handles["elm"],
        made.handles["key"],
    );

    // The director rules on the first kick and makes the key notable.
    client.push(Response::canned_tool_use(
        "k1",
        "mint_rule",
        crate::game::tests::kick_mint_input(oak),
    ));
    turn(
        &mut game,
        &client,
        &[learn_call(
            json!({"name": "iron key", "category": "item", "subject": key, "facts": [
                {"text": "An iron key lies by the font.", "excerpt": "an iron key by the font"}
            ]}),
        )],
        "The oak door bursts. Old Hal flinches; you spot an iron key by the font.",
    );
    let minted = game
        .world()
        .events()
        .unwrap()
        .into_iter()
        .rev()
        .find(|e| e.kind == EventKind::ToolCall && e.payload["name"] == "mint_rule")
        .unwrap();
    let rule =
        serde_json::from_str::<Value>(minted.payload["result"].as_str().unwrap()).unwrap()["rule"]
            .as_i64()
            .unwrap();
    let hal = place(game.world(), "Old Hal");
    assert_eq!(hal.encounters, 1);
    assert_eq!(place(game.world(), "iron key").encounters, 1);

    // The repeat goes the fast path: interpreter and narrator, no more.
    let before = client.requests().len();
    client.push(interpret(
        json!([{"rule": rule, "bindings": {"target": elm}}]),
    ));
    client.push(Response::canned_text(
        "The elm door gives too. Old Hal shakes his head; the iron key glints.",
    ));
    let out = game
        .turn(&client, "kick the elm door", &mut |_| {})
        .unwrap();
    assert_eq!(out.fast_path, Some(rule));
    assert_eq!(client.requests().len() - before, 2);
    let world = game.world();
    let seen = narration_seq(world);
    for name in ["Old Hal", "iron key"] {
        let e = place(world, name);
        assert_eq!((e.encounters, e.last_seen), (2, seen), "{name}");
    }
    // Neither door was notable: the engine makes entries only for people.
    assert!(
        entries(world)
            .unwrap()
            .iter()
            .all(|e| !e.name.contains("door"))
    );
    let all = every_report(world);
    assert!(!all.contains(SECRET) && !all.contains("crypt"), "{all}");
}

#[test]
fn only_observed_movement_updates_last_seen_and_destruction_keeps_the_entry() {
    let (mut game, client) = begun();
    let chapel = player_room(&game);
    let hal = npc(&mut game, "Old Hal", chapel);
    turn(&mut game, &client, &[], "Old Hal mends a net by the altar.");
    let chapel_entry = place(game.world(), "Drowned Chapel").id;
    let first = place(game.world(), "Old Hal");
    assert_eq!(first.seen_at, Some(chapel_entry));

    // The player goes north; Hal is moved off-screen behind them.
    client.push(tower_room());
    turn(&mut game, &client, &[], "You climb into the bell tower.");
    let tower = player_room(&game);
    let tower_entry = place(game.world(), "Bell Tower").id;
    patch(
        &mut game,
        json!([{"op": "set_prop", "entity": hal, "key": "mood", "value": "furious"}]),
    );
    patch(
        &mut game,
        json!([{"op": "create", "handle": "b", "name": "punt", "kind": "boat", "location": chapel,
                "props": {"container": true}},
               {"op": "move", "entity": hal, "to": "b"}]),
    );
    // A tick off-screen changes nothing the player knows.
    let known = entries(game.world()).unwrap();
    client.push(Response::canned_tool_use(
        "k",
        "apply_patch",
        json!({"note": "Hal sulks.", "ops": [
            {"op": "set_prop", "entity": hal, "key": "mood", "value": "sulking"}
        ]}),
    ));
    client.push(Response::canned_text("Hal sulks in the loft."));
    let before = client.requests().len();
    game.tick(&client).unwrap();
    assert!(
        client.requests()[before]
            .tools
            .iter()
            .all(|t| t.name != "learn" && t.name != "recall"),
        "ticks can't learn"
    );
    assert_eq!(entries(game.world()).unwrap(), known);
    turn(
        &mut game,
        &client,
        &[],
        "Wind moans through the tower. Old Hal is nowhere.",
    );
    let e = place(game.world(), "Old Hal");
    assert_eq!(
        e.seen_at,
        Some(chapel_entry),
        "off-screen moves aren't seen"
    );
    assert_eq!(e.last_seen, first.last_seen);

    // He climbs in where the player can see him.
    patch(
        &mut game,
        json!([{"op": "move", "entity": hal, "to": tower}]),
    );
    turn(
        &mut game,
        &client,
        &[],
        "Old Hal hauls himself up into the tower.",
    );
    let seen = narration_seq(game.world());
    let e = place(game.world(), "Old Hal");
    assert_eq!(
        (e.seen_at, e.last_seen, e.encounters),
        (Some(tower_entry), seen, 2)
    );
    let detail = compendium(game.world(), "Old Hal");
    assert!(
        detail.contains(&format!("Last seen at Bell Tower [#{seen}].")),
        "{detail}"
    );

    // Destroyed, he is still remembered as he was.
    patch(&mut game, json!([{"op": "destroy", "entity": hal}]));
    turn(
        &mut game,
        &client,
        &[],
        "The tower is empty but for the wind.",
    );
    assert_eq!(place(game.world(), "Old Hal"), e);
    assert_eq!(compendium(game.world(), "Old Hal"), detail);
    assert!(compendium(game.world(), "search Hal").contains(&format!("#{} Old Hal", e.id)));
}

#[test]
fn npcs_opens_the_player_people_report() {
    let (mut game, client, _) = hal_mentioned();
    let chapel = player_room(&game);
    npc(&mut game, "Brother Ash", chapel);
    npc(&mut game, "lurker", chapel);
    turn(&mut game, &client, &[], "Brother Ash nods to you.");
    let world = game.world();
    let npcs = commands::report(world, ":npcs", "").unwrap().unwrap();
    assert_eq!(npcs, compendium(world, "people"));
    assert!(npcs.contains("Old Hal (person)") && npcs.contains("Brother Ash (person)"));
    assert!(!npcs.contains("lurker") && !npcs.contains(SECRET), "{npcs}");
    // Search and detail are just as limited.
    assert!(compendium(world, "search lurker").starts_with("Nothing you know"));
    assert!(compendium(world, "lurker").starts_with("You know of nothing"));
}

#[test]
fn links_corrections_and_encounters_survive_reopen_replay_fork_and_an_adopted_reroll() {
    use crate::branches::tests::temp_dir;
    let dir = temp_dir("knowledge-encounters");
    let (mut game, client, entry) = hal_mentioned();
    let chapel = player_room(&game);
    let hal = npc(&mut game, "Old Hal", chapel);
    turn(
        &mut game,
        &client,
        &[learn_call(
            json!({"entry": entry, "subject": hal, "facts": [
                {"text": "Old Hal is alive.", "excerpt": "Old Hal is alive", "supersedes": [1]}
            ]}),
        )],
        "Old Hal is alive, and waves.",
    );
    turn(&mut game, &client, &[], "Old Hal coils his net.");
    let want = entries(game.world()).unwrap();
    assert_eq!(place(game.world(), "Old Hal").encounters, 2);
    let path = dir.join("hal.db");
    let last = game.world().last_seq().unwrap();
    drop(game.world().fork(last, &path).unwrap());
    drop(game);

    let reopened = World::open(&path).unwrap();
    assert_eq!(entries(&reopened).unwrap(), want);
    assert_eq!(
        entries(&reopened.replay_in_memory().unwrap()).unwrap(),
        want
    );
    let fork = reopened.fork(last, dir.join("fork.db")).unwrap();
    assert_eq!(entries(&fork).unwrap(), want);

    // Reroll the last turn; the adopted variant's narration doesn't name
    // Hal, so its branch has one encounter fewer, and replays to itself.
    let game = Game::new(reopened);
    let turn_id = crate::xray::latest_turn(game.world()).unwrap().unwrap();
    let scripts = [ScriptedClient::new([Response::canned_text(
        "The net lies coiled.",
    )])];
    let spec = crate::reroll::Spec {
        turn: Some(turn_id),
        n: 1,
        ..crate::reroll::Spec::default()
    };
    let r = crate::reroll::run(
        game.world(),
        &spec,
        game.brains(),
        &|n| &scripts[n - 1] as &(dyn crate::llm::LlmClient + Sync),
        &mut |_| {},
    )
    .unwrap();
    let adopted = dir.join("adopted.db");
    r.adopt(1, &adopted).unwrap();
    let branch = World::open(&adopted).unwrap();
    let e = place(&branch, "Old Hal");
    assert_eq!((e.id, e.encounters, e.superseded()), (entry, 1, vec![1]));
    assert_eq!(
        entries(&branch.replay_in_memory().unwrap()).unwrap(),
        entries(&branch).unwrap()
    );
    assert_eq!(entries(game.world()).unwrap(), want);
    let _ = std::fs::remove_dir_all(&dir);
}
