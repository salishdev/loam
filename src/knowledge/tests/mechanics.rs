//! Learned mechanics: interactions and terms the player discovers in play,
//! linked to the rules and glossary terms behind them without revealing
//! them.

use serde_json::Value;

use super::*;
use crate::glossary::Definition;
use crate::llm::ContentBlock;
use crate::world::Patch;

const RULING: &str = "You drive your boot into the oak door. The door bursts inward in wet \
splinters, soft as old bread.";

/// Doors in the player's room: oak, elm, ash.
fn add_doors(game: &mut Game) -> [EntityId; 3] {
    let room = player_room(game);
    let ops = serde_json::from_value(json!([
        {"op": "create", "handle": "oak", "name": "oak door", "kind": "door", "location": room, "props": {"rotten": true}},
        {"op": "create", "handle": "elm", "name": "elm door", "kind": "door", "location": room, "props": {"rotten": true}},
        {"op": "create", "handle": "ash", "name": "ash door", "kind": "door", "location": room, "props": {"rotten": true}}
    ]))
    .unwrap();
    let a = game.world_mut().apply_patch(&Patch::new(ops)).unwrap();
    [a.handles["oak"], a.handles["elm"], a.handles["ash"]]
}

/// Play a director turn from `input`: the interpreter (asked whenever an
/// active rule exists) matches nothing, then the director gives `responses`.
fn director_turn(game: &mut Game, client: &ScriptedClient, responses: Vec<Response>, input: &str) {
    if !crate::rules::active_world(game.world()).unwrap().is_empty() {
        client.push(Response::canned_tool_use(
            "i",
            "interpret",
            json!({"matches": [], "clean": false, "duration": "minutes"}),
        ));
    }
    for r in responses {
        client.push(r);
    }
    game.turn(client, input, &mut |_| {}).unwrap();
}

/// The director rules on kicking the oak door, minting a rule (pinned to
/// that door with `pin`) that hides a condition and side effects. Returns
/// the rule and the ruling's narration event.
fn rule_on_kick(game: &mut Game, client: &ScriptedClient, oak: EntityId, pin: bool) -> (i64, i64) {
    let mut input = crate::game::tests::kick_mint_input(oak);
    if pin {
        input["conditions"]
            .as_array_mut()
            .unwrap()
            .push(json!({"check": "entity", "role": "target", "id": oak}));
    }
    director_turn(
        game,
        client,
        vec![
            Response::canned_tool_use("k", "mint_rule", input),
            Response::canned_text(RULING),
        ],
        "kick the oak door",
    );
    let rule = crate::rules::all(game.world())
        .unwrap()
        .into_iter()
        .find(|r| r.def.name == "Kicking a rotten door")
        .unwrap()
        .id;
    let narration = game
        .world()
        .last_event(EventKind::Narration)
        .unwrap()
        .unwrap()
        .seq;
    (rule, narration)
}

/// A turn whose director makes the `learn` call `input`, then narrates
/// `narration`. Returns the learn call's result.
fn learn_turn(
    game: &mut Game,
    client: &ScriptedClient,
    input: Value,
    narration: &str,
) -> (String, bool) {
    director_turn(
        game,
        client,
        vec![
            Response::canned_tool_use("l", "learn", input),
            Response::canned_text(narration),
        ],
        "think it over",
    );
    tool_result(client, 1)
}

/// The `n`th-from-last request's tool result.
fn tool_result(client: &ScriptedClient, back: usize) -> (String, bool) {
    let requests = client.requests();
    match requests[requests.len() - back]
        .messages
        .last()
        .unwrap()
        .content
        .first()
    {
        Some(ContentBlock::ToolResult {
            content, is_error, ..
        }) => (content.clone(), *is_error),
        other => panic!("expected a tool result, got {other:?}"),
    }
}

/// Learn the kick rule as a mechanic, citing the ruling's narration.
fn learn_kick(game: &mut Game, client: &ScriptedClient, rule: i64, ruling: i64) -> Entry {
    let (msg, is_error) = learn_turn(
        game,
        client,
        json!({"name": "Rotten doors give way", "category": "mechanic", "rule": rule, "facts": [
            {"text": "A rotten door bursts open when kicked.", "excerpt": "The door bursts inward in wet splinters", "source": ruling}
        ]}),
        "You remember how the oak door gave way.",
    );
    assert!(!is_error, "{msg}");
    place(game.world(), "Rotten doors give way")
}

/// Everything a player could read about mechanics.
fn player_text(world: &World) -> String {
    let mut out = String::new();
    for (cmd, arg) in [
        (":rules", ""),
        (":glossary", ""),
        (":compendium", ""),
        (":compendium", "mechanics"),
    ] {
        out.push_str(&commands::report(world, cmd, arg).unwrap().unwrap());
        out.push('\n');
    }
    for e in entries(world).unwrap() {
        out.push_str(&compendium(world, &format!("#{}", e.id)));
        out.push('\n');
    }
    out
}

/// Words only the engine's rules and glossary hold.
const HIDDEN: [&str; 8] = [
    "kick, shoulder or force", // a trigger
    "not_prop",                // a condition
    "broken",                  // a condition and effect prop
    "debris",                  // an unobserved created entity
    "damp",                    // and its prop
    "Secret lever",            // a rule never learned
    "hidden_switch",           // its condition's prop
    "Wood gone soft",          // a glossary meaning
];

fn assert_hides_engine(world: &World) {
    let text = player_text(world);
    for word in HIDDEN {
        assert!(!text.contains(word), "{word:?} leaked into:\n{text}");
        assert!(
            search(world, word).unwrap().is_empty(),
            "search found {word:?}"
        );
        assert!(
            matches!(find(world, word).unwrap(), Found::None),
            "{word:?}"
        );
    }
}

/// A rule the player never meets, and a glossary meaning never learned.
fn unlearned_engine_state(game: &mut Game) {
    let def = serde_json::from_value(json!({
        "name": "Secret lever",
        "trigger": "pull the lever",
        "conditions": [{"check": "prop", "role": "room", "prop": "hidden_switch", "is": true}],
        "effects": [],
        "outcome": "A wall slides aside."
    }))
    .unwrap();
    crate::rules::mint(game.world_mut(), def, "a secret")
        .unwrap()
        .unwrap();
    crate::glossary::define(
        game.world_mut(),
        &[Definition {
            name: "rotten".into(),
            meaning: "Wood gone soft enough to break".into(),
            values: Default::default(),
            free_text: None,
        }],
    )
    .unwrap()
    .unwrap();
}

#[test]
fn a_narrated_ruling_becomes_a_readable_sourced_mechanic_and_nothing_unlearned_shows() {
    let (mut game, client) = begun();
    let [oak, ..] = add_doors(&mut game);
    unlearned_engine_state(&mut game);
    let (rule, ruling) = rule_on_kick(&mut game, &client, oak, false);

    // Rules and glossary terms exist, but nothing about them is learned.
    let world = game.world();
    assert!(mechanics(world).unwrap().interactions.is_empty());
    assert!(
        commands::report(world, ":rules", "")
            .unwrap()
            .unwrap()
            .starts_with("No interactions known yet")
    );
    assert!(
        commands::report(world, ":glossary", "")
            .unwrap()
            .unwrap()
            .starts_with("No terms known yet")
    );
    assert!(compendium(world, "mechanics").starts_with("No mechanics known yet"));
    assert!(search(world, "Kicking").unwrap().is_empty());
    assert_hides_engine(world);

    let e = learn_kick(&mut game, &client, rule, ruling);
    assert_eq!(e.category, Category::Mechanic);
    assert_eq!(e.link, Some(Link::Rule(rule)));
    assert_eq!(e.summary, "A rotten door bursts open when kicked.");
    assert_eq!(e.facts[0].rule_version, Some(1));
    assert_eq!(e.facts[0].sources[0].event, ruling);
    let world = game.world();
    let rules = commands::report(world, ":rules", "").unwrap().unwrap();
    assert!(
        rules.contains("Rotten doors give way · mechanic"),
        "{rules}"
    );
    assert!(
        rules.contains(&format!(
            "1. A rotten door bursts open when kicked. [#{ruling} \"The door bursts inward in wet splinters\"]"
        )),
        "{rules}"
    );
    assert!(compendium(world, "mechanics").contains("Interactions\n    #"));
    // The rule's own name isn't what the player learned it as.
    assert!(!rules.contains("Kicking a rotten door"), "{rules}");
    assert_hides_engine(world);

    // A term, learned from what was narrated: its glossary meaning stays out.
    let (msg, is_error) = learn_turn(
        &mut game,
        &client,
        json!({"name": "Rot", "category": "mechanic", "term": "rotten", "facts": [
            {"text": "Rot makes wood soft.", "excerpt": "soft as old bread", "source": ruling}
        ]}),
        "You think about rot.",
    );
    assert!(!is_error, "{msg}");
    let world = game.world();
    let terms = commands::report(world, ":glossary", "").unwrap().unwrap();
    assert!(terms.contains("Rot · mechanic") && terms.contains("Rot makes wood soft."));
    assert!(!terms.contains("Rotten doors"), "{terms}");
    let m = mechanics(world).unwrap();
    assert_eq!(m.terms.len(), 1);
    assert_eq!(m.interactions.len(), 1);
    assert!(compendium(world, "mechanics").contains("  Terms\n    #"));
    assert_hides_engine(world);
}

#[test]
fn links_are_checked_and_never_shared() {
    let (mut game, client) = begun();
    let [oak, ..] = add_doors(&mut game);
    let (rule, ruling) = rule_on_kick(&mut game, &client, oak, false);
    let fact =
        json!([{"text": "Doors give.", "excerpt": "The door bursts inward", "source": ruling}]);
    let refused = |game: &mut Game, input: Value| {
        let (msg, is_error) = learn_turn(game, &client, input, "You wait.");
        assert!(is_error, "accepted: {msg}");
        msg
    };
    assert!(
        refused(
            &mut game,
            json!({"name": "Doors", "category": "lore", "rule": rule, "facts": fact})
        )
        .contains("go with a mechanic")
    );
    assert!(
        refused(
            &mut game,
            json!({"name": "Doors", "category": "mechanic", "rule": 9999, "facts": fact})
        )
        .contains("no rule #9999")
    );
    assert!(
        refused(
            &mut game,
            json!({"name": "Doors", "category": "mechanic", "term": "nonsense", "facts": fact})
        )
        .contains("not a glossary term")
    );
    learn_kick(&mut game, &client, rule, ruling);
    let msg = refused(
        &mut game,
        json!({"name": "Kicking", "category": "mechanic", "rule": rule, "facts": fact}),
    );
    assert!(msg.contains("already about rule"), "{msg}");
    assert_eq!(mechanics(game.world()).unwrap().interactions.len(), 1);

    // Two calls in one turn can't link one entry to different things: the
    // second is refused when made, not dropped at the end of the turn.
    let unlinked = learn_turn(
        &mut game,
        &client,
        json!({"name": "Doors", "category": "mechanic", "facts": fact}),
        "You wait.",
    );
    assert!(!unlinked.1, "{}", unlinked.0);
    let doors = place(game.world(), "Doors").id;
    let other = crate::rules::all(game.world()).unwrap()[0].id;
    assert_ne!(other, rule);
    director_turn(
        &mut game,
        &client,
        vec![
            Response::canned_tool_use(
                "a",
                "learn",
                json!({"entry": doors, "rule": other, "facts": fact}),
            ),
            Response::canned_tool_use(
                "b",
                "learn",
                json!({"entry": doors, "term": "rotten", "facts": fact}),
            ),
            Response::canned_text("You wait."),
        ],
        "wait",
    );
    let (msg, is_error) = tool_result(&client, 1);
    assert!(is_error && msg.contains("links entry"), "{msg}");
    assert_eq!(place(game.world(), "Doors").link, Some(Link::Rule(other)));
}

#[test]
fn fast_path_use_of_a_learned_mechanic_is_recorded_without_more_calls_or_hidden_effects() {
    let (mut game, client) = begun();
    let [oak, elm, ash] = add_doors(&mut game);
    unlearned_engine_state(&mut game);
    let (rule, ruling) = rule_on_kick(&mut game, &client, oak, false);
    let fast = |game: &mut Game, door: EntityId| {
        let start = client.requests().len();
        client.push(Response::canned_tool_use(
            "i",
            "interpret",
            json!({"matches": [{"rule": rule, "bindings": {"target": door}}], "clean": true, "duration": "moment"}),
        ));
        client.push(Response::canned_text("Another door splinters."));
        let out = game.turn(&client, "kick that door", &mut |_| {}).unwrap();
        assert_eq!(out.fast_path, Some(rule));
        // The interpreter and the narrator only: no director, no extraction.
        assert_eq!(client.requests().len() - start, 2);
        game.world()
            .last_event(EventKind::Narration)
            .unwrap()
            .unwrap()
            .seq
    };

    // Using a rule the player never learned teaches them nothing.
    fast(&mut game, elm);
    assert!(mechanics(game.world()).unwrap().interactions.is_empty());

    let learned = learn_kick(&mut game, &client, rule, ruling);
    assert_eq!(learned.usage, None);
    let used = fast(&mut game, ash);
    let e = place(game.world(), "Rotten doors give way");
    assert_eq!(
        e.usage,
        Some(Usage {
            count: 1,
            first: used,
            last: used,
            version: 1
        })
    );
    // Nothing more about the rule was learned.
    assert_eq!(e.facts, learned.facts);
    assert_eq!(e.summary, learned.summary);
    let detail = compendium(game.world(), "Rotten doors give way");
    assert!(
        detail.contains(&format!("Used once: first #{used}, last #{used}.")),
        "{detail}"
    );
    assert_hides_engine(game.world());
    assert_eq!(
        entries(&game.world().replay_in_memory().unwrap()).unwrap(),
        entries(game.world()).unwrap()
    );
}

#[test]
fn changing_a_linked_rule_keeps_what_was_learned_until_narration_supersedes_it() {
    let (mut game, client) = begun();
    let [oak, ..] = add_doors(&mut game);
    let (rule, ruling) = rule_on_kick(&mut game, &client, oak, false);
    let learned = learn_kick(&mut game, &client, rule, ruling);

    // The director amends the rule: the learned account stays as it was,
    // and the director is shown what the player learned.
    director_turn(
        &mut game,
        &client,
        vec![
            Response::canned_tool_use(
                "a",
                "amend_rule",
                json!({"id": rule, "reason": "doors now stick", "outcome": "The door shudders but holds."}),
            ),
            Response::canned_text("Somewhere a door shudders."),
        ],
        "listen",
    );
    let (msg, is_error) = tool_result(&client, 1);
    assert!(!is_error, "{msg}");
    let result: Value = serde_json::from_str(&msg).unwrap();
    assert_eq!(result["version"], 2);
    assert_eq!(result["player_learned"]["entry"], learned.id);
    assert_eq!(
        result["player_learned"]["facts"],
        json!([{"fact": 1, "text": "A rotten door bursts open when kicked.", "learned_at_rule_version": 1}])
    );
    let e = place(game.world(), "Rotten doors give way");
    assert_eq!(e.facts, learned.facts);
    assert_eq!(e.summary, learned.summary);

    // Later narration shows the change: the new fact supersedes the old one,
    // which keeps its sources.
    let narration = "You kick the elm door. The door shudders but holds against your boot.";
    let (msg, is_error) = learn_turn(
        &mut game,
        &client,
        json!({"entry": learned.id, "facts": [
            {"text": "A rotten door now holds when kicked.", "excerpt": "The door shudders but holds", "supersedes": [1]}
        ]}),
        narration,
    );
    assert!(!is_error, "{msg}");
    let now = game
        .world()
        .last_event(EventKind::Narration)
        .unwrap()
        .unwrap()
        .seq;
    let e = place(game.world(), "Rotten doors give way");
    assert_eq!(e.facts.len(), 2);
    assert_eq!(e.facts[0].text, learned.facts[0].text);
    assert_eq!(e.facts[0].sources, learned.facts[0].sources);
    assert_eq!(e.facts[0].rule_version, Some(1));
    assert_eq!(e.superseded(), [1]);
    assert_eq!(e.facts[1].rule_version, Some(2));
    assert_eq!(e.facts[1].sources[0].event, now);
    assert_eq!(e.summary, "A rotten door now holds when kicked.");
    let detail = compendium(game.world(), "Rotten doors give way");
    assert!(
        detail.contains(&format!(
            "1. A rotten door bursts open when kicked. (superseded by 2) [#{ruling}"
        )),
        "{detail}"
    );

    // Only facts the entry has can be superseded.
    {
        let n = 9;
        let (msg, is_error) = learn_turn(
            &mut game,
            &client,
            json!({"entry": learned.id, "facts": [
                {"text": "Doors are odd.", "excerpt": "You wait here", "supersedes": n}
            ]}),
            "You wait here.",
        );
        assert!(is_error, "{msg}");
    }

    // Retiring the rule doesn't touch what was learned either.
    let before = place(game.world(), "Rotten doors give way");
    director_turn(
        &mut game,
        &client,
        vec![
            Response::canned_tool_use(
                "r",
                "retire_rule",
                json!({"id": rule, "reason": "no more doors"}),
            ),
            Response::canned_text("Time passes."),
        ],
        "wait",
    );
    let (msg, _) = tool_result(&client, 1);
    assert_eq!(
        serde_json::from_str::<Value>(&msg).unwrap()["player_learned"]["entry"],
        before.id
    );
    assert_eq!(place(game.world(), "Rotten doors give way"), before);
}

#[test]
fn rules_and_glossary_read_learned_mechanics_with_no_model_call_or_write() {
    let (mut game, client) = begun();
    let [oak, ..] = add_doors(&mut game);
    let (rule, ruling) = rule_on_kick(&mut game, &client, oak, false);
    learn_kick(&mut game, &client, rule, ruling);
    let world = game.world();
    let calls = client.requests().len();
    let seq = world.last_seq().unwrap();
    let state = world.state().unwrap();
    assert_eq!(
        commands::report(world, ":rules", "").unwrap().unwrap(),
        learned_rules(world).unwrap()
    );
    assert_eq!(
        commands::report(world, ":glossary", "").unwrap().unwrap(),
        learned_terms(world).unwrap()
    );
    player_text(world);
    mechanics(world).unwrap();
    assert_eq!(client.requests().len(), calls);
    assert_eq!(world.last_seq().unwrap(), seq);
    assert_eq!(world.state().unwrap(), state);
}

#[test]
fn mechanics_survive_reopen_replay_fork_and_an_adopted_reroll_after_their_rule_retires() {
    use crate::branches::tests::{saved_game, temp_dir};
    let dir = temp_dir("knowledge-mechanics");
    let path = saved_game(&dir, "mill.db");
    let mut game = Game::new(World::open(&path).unwrap());
    let client = ScriptedClient::new([]);
    let [oak, ..] = add_doors(&mut game);
    let (rule, ruling) = rule_on_kick(&mut game, &client, oak, true);
    let learned = learn_kick(&mut game, &client, rule, ruling);

    // Destroying the door the rule is pinned to retires the rule.
    director_turn(
        &mut game,
        &client,
        vec![
            Response::canned_tool_use(
                "d",
                "apply_patch",
                json!({"ops": [{"op": "destroy", "entity": oak}]}),
            ),
            Response::canned_text("The oak door crumbles to nothing."),
        ],
        "wait",
    );
    let retired = crate::rules::get(game.world(), rule).unwrap().unwrap();
    assert!(!retired.is_active(), "the pinned rule retires");
    assert_eq!(place(game.world(), "Rotten doors give way"), learned);
    let want = entries(game.world()).unwrap();
    let last = game.world().last_seq().unwrap();
    drop(game);

    let reopened = World::open(&path).unwrap();
    assert_eq!(entries(&reopened).unwrap(), want);
    assert_eq!(
        entries(&reopened.replay_in_memory().unwrap()).unwrap(),
        want
    );
    let fork = reopened.fork(last, dir.join("fork.db")).unwrap();
    assert_eq!(entries(&fork).unwrap(), want);
    drop(fork);

    // Reroll the destroying turn and adopt a variant: the mechanic, its
    // evidence and its rule-version link come along.
    let game = Game::new(reopened);
    let turn = crate::xray::latest_turn(game.world()).unwrap().unwrap();
    // The rule was active when the turn began, so the variant asks the
    // interpreter first.
    let scripts = [ScriptedClient::new([
        Response::canned_tool_use(
            "i",
            "interpret",
            json!({"matches": [], "clean": false, "duration": "minutes"}),
        ),
        Response::canned_text("Nothing happens."),
    ])];
    let spec = crate::reroll::Spec {
        turn: Some(turn),
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
    assert_eq!(place(&branch, "Rotten doors give way"), learned);
    assert_eq!(
        entries(&branch.replay_in_memory().unwrap()).unwrap(),
        entries(&branch).unwrap()
    );
    drop(branch);
    let _ = std::fs::remove_dir_all(&dir);
}
