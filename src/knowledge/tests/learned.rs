//! Discoveries the director records with `learn` during play.

use super::*;
use crate::llm::{ContentBlock, StopReason};

fn learn_call(input: Value) -> Response {
    Response::canned_tool_use("l", "learn", input)
}

use serde_json::Value;

/// Play a turn: each `learn` input in turn, then `narration`.
fn learning_turn(game: &mut Game, client: &ScriptedClient, learns: &[Value], narration: &str) {
    for l in learns {
        client.push(learn_call(l.clone()));
    }
    client.push(Response::canned_text(narration));
    game.turn(client, "listen to the monk", &mut |_| {})
        .unwrap();
}

/// The tool results the director got back on the latest turn, in order.
fn tool_results(client: &ScriptedClient, last: usize) -> Vec<(String, bool)> {
    let requests = client.requests();
    requests[requests.len() - last..]
        .iter()
        .filter_map(|r| match r.messages.last()?.content.first()? {
            ContentBlock::ToolResult {
                content, is_error, ..
            } => Some((content.clone(), *is_error)),
            _ => None,
        })
        .collect()
}

fn one(world: &World, name: &str) -> Entry {
    place(world, name)
}

/// World entities other than compendium entries, without the clock.
fn world_entities(world: &World) -> Vec<crate::world::Entity> {
    world
        .state()
        .unwrap()
        .entities
        .into_iter()
        .filter(|e| !crate::world::is_entry(e))
        .map(|mut e| {
            e.props.remove("_clock");
            e
        })
        .collect()
}

const ASH_NARRATION: &str = "A gaunt monk ties off the bell rope. \"Brother Ash,\" he says. \
\"The eel-wives drowned the bell,\" he whispers, \"or so the ferrymen say.\"";

fn ash_and_the_bell() -> Vec<Value> {
    vec![
        json!({"name": "Brother Ash", "category": "person", "facts": [
            {"text": "A gaunt monk who tends the bell rope.", "excerpt": "A gaunt monk ties off the bell rope"},
            {"text": "Calls himself Brother Ash.", "excerpt": "\u{201c}Brother Ash,\u{201d} he says"}
        ]}),
        json!({"name": "The drowned bell", "category": "lore", "facts": [
            {"text": "The eel-wives drowned the bell.", "excerpt": "the eel-wives drowned the bell",
             "claimed_by": "Brother Ash"}
        ]}),
    ]
}

#[test]
fn narrated_observations_and_named_claims_become_readable_entries() {
    let (mut game, client) = begun();
    let before = client.requests().len();
    learning_turn(&mut game, &client, &ash_and_the_bell(), ASH_NARRATION);
    // The director's own calls (two learns, then the narration) and no
    // extraction call after it.
    assert_eq!(client.requests().len() - before, 3);
    assert!(
        client.requests()[before..]
            .iter()
            .all(|r| r
                .messages
                .iter()
                .all(|m| m.content.iter().all(|b| !matches!(
                    b,
                    ContentBlock::Text { text } if text.contains(ASH_NARRATION)
                )))),
        "no call reads the narration back"
    );
    let world = game.world();
    let narration = world.last_event(EventKind::Narration).unwrap().unwrap().seq;

    let ash = one(world, "Brother Ash");
    assert_eq!(ash.category, Category::Person);
    assert_eq!(
        ash.summary,
        "A gaunt monk who tends the bell rope. Calls himself Brother Ash."
    );
    assert_eq!(ash.encounters, 0);
    assert!(ash.facts.iter().all(|f| f.kind == FactKind::Observed));
    assert_eq!(
        ash.facts[0].sources,
        [Source {
            event: narration,
            excerpt: Some("A gaunt monk ties off the bell rope".into())
        }]
    );

    // The rumour stays a claim: attributed, and not in the summary.
    let bell = one(world, "drowned bell");
    assert_eq!(bell.category, Category::Lore);
    assert_eq!(bell.facts[0].kind, FactKind::Claimed);
    assert_eq!(bell.facts[0].claimed_by.as_deref(), Some("Brother Ash"));
    assert_eq!(bell.summary, "");
    let detail = compendium(world, "drowned bell");
    assert!(
        detail.contains("Known only from what others say."),
        "{detail}"
    );
    assert!(
        detail.contains(&format!(
            "- Brother Ash claims: The eel-wives drowned the bell. [#{narration} \"the eel-wives drowned the bell\"]"
        )),
        "{detail}"
    );
    assert!(detail.contains("not encountered"), "{detail}");
    let detail = compendium(world, "Brother Ash");
    assert!(detail.starts_with(&format!("Brother Ash · person · entry #{}", ash.id)));
    assert!(detail.contains(&format!(
        "- A gaunt monk who tends the bell rope. [#{narration} \"A gaunt monk ties off the bell rope\"]"
    )));

    // Later the claim is repeated with its claimant linked to Ash's entry.
    let mut game = game;
    learning_turn(
        &mut game,
        &client,
        &[json!({"entry": bell.id, "facts": [
            {"text": "The eel-wives drowned the bell.", "excerpt": "the eel-wives did it",
             "claimed_by": "Brother Ash", "claimant": ash.id}
        ]})],
        "Ash repeats it: the eel-wives did it.",
    );
    let bell = one(game.world(), "drowned bell");
    assert_eq!(bell.facts.len(), 1, "the same claim, cited twice");
    assert_eq!(bell.facts[0].sources.len(), 2);
    assert_eq!(bell.facts[0].claimant, Some(ash.id));
    assert!(
        compendium(game.world(), "drowned bell")
            .contains(&format!("Brother Ash (#{}) claims:", ash.id))
    );
}

#[test]
fn a_mention_makes_an_entry_without_a_world_entity_and_same_names_stay_distinct() {
    let (mut game, client) = begun();
    let world_before = world_entities(game.world());
    learning_turn(
        &mut game,
        &client,
        &[
            json!({"name": "Mara", "category": "person", "facts": [
                {"text": "Ferries travellers across the fen.", "excerpt": "Mara ferries travellers across",
                 "claimed_by": "a carved sign"}
            ]}),
            json!({"name": "Salt Tower", "category": "place", "facts": [
                {"text": "Stands beyond the reeds.", "excerpt": "the Salt Tower stands beyond"}
            ]}),
        ],
        "A carved sign: Mara ferries travellers across. Far off, the Salt Tower stands beyond the reeds.",
    );
    assert_eq!(
        world_entities(game.world()),
        world_before,
        "a mention creates nothing in the world"
    );
    assert_eq!(
        names(game.world()),
        ["Drowned Chapel", "Mara", "Salt Tower"]
    );
    let ferrywoman = one(game.world(), "Mara");

    // Another Mara: refused until the director says which.
    let calls = client.requests().len();
    learning_turn(
        &mut game,
        &client,
        &[
            json!({"name": "mara", "category": "person", "facts": [
                {"text": "A child selling eels.", "excerpt": "a child named Mara"}]}),
            json!({"name": "Mara", "category": "person", "distinct": true, "facts": [
                {"text": "A child selling eels.", "excerpt": "a child named Mara"}]}),
            json!({"entry": ferrywoman.id, "facts": [
                {"text": "Owes the eel-wives a debt.", "excerpt": "the ferrywoman owes the eel-wives",
                 "claimed_by": "the child"}]}),
        ],
        "A child named Mara sells eels; she says the ferrywoman owes the eel-wives.",
    );
    let results = tool_results(&client, client.requests().len() - calls);
    assert!(results[0].1, "same name refused: {results:?}");
    assert!(
        results[0]
            .0
            .contains(&format!("#{} (Ferries travellers", ferrywoman.id)),
        "{results:?}"
    );
    assert!(!results[1].1 && !results[2].1, "{results:?}");
    let world = game.world();
    let maras = match find(world, "Mara").unwrap() {
        Found::Many(m) => m,
        other => panic!("{other:?}"),
    };
    assert_eq!(maras.len(), 2);
    let child = maras.iter().find(|m| m.id != ferrywoman.id).unwrap();
    assert_eq!(child.summary, "A child selling eels.");
    let ferry = entry(world, ferrywoman.id).unwrap().unwrap();
    assert_eq!(ferry.facts.len(), 2, "the debt went to the named entry");
    assert!(child.facts.iter().all(|f| !f.text.contains("debt")));
    // Each is readable by its own id.
    assert!(compendium(world, &format!("#{}", child.id)).contains("A child selling eels."));
    assert!(compendium(world, "Mara").contains("could be several entries"));
    assert_eq!(world_entities(world).len(), world_before.len());
}

#[test]
fn an_entry_can_be_linked_to_a_world_entity_without_reading_it() {
    let (mut game, client) = begun();
    let altar = game
        .world()
        .contents(Some(super::player_room(&game)))
        .unwrap()
        .into_iter()
        .find(|&id| game.world().entity(id).unwrap().unwrap().name == "altar")
        .unwrap();
    learning_turn(
        &mut game,
        &client,
        &[
            json!({"name": "Old altar", "category": "item", "subject": altar, "facts": [
            {"text": "Cut from dark stone.", "excerpt": "the altar is dark stone"}]}),
        ],
        "You see the altar is dark stone.",
    );
    let detail = compendium(game.world(), "Old altar");
    // The altar's own props (material: slate) and contents stay unknown.
    for hidden in ["slate", "candle", "altar (feature)", &format!("#{altar} ")] {
        assert!(!detail.contains(hidden), "{hidden}: {detail}");
    }
    // A second entry about the same entity is refused.
    let calls = client.requests().len();
    learning_turn(
        &mut game,
        &client,
        &[
            json!({"name": "Stone table", "category": "item", "subject": altar, "facts": [
            {"text": "Wet.", "excerpt": "the table is wet"}]}),
        ],
        "The table is wet.",
    );
    let results = tool_results(&client, client.requests().len() - calls);
    assert!(
        results[0].1 && results[0].0.contains("already about"),
        "{results:?}"
    );
    assert_eq!(names(game.world()), ["Drowned Chapel", "Old altar"]);
}

#[test]
fn citations_must_be_narration_the_player_saw_and_contain_the_excerpt() {
    let (mut game, client) = begun();
    learning_turn(&mut game, &client, &ash_and_the_bell(), ASH_NARRATION);
    let narration = game
        .world()
        .last_event(EventKind::Narration)
        .unwrap()
        .unwrap()
        .seq;
    let input = game
        .world()
        .last_event(EventKind::Input)
        .unwrap()
        .unwrap()
        .seq;
    let tool_call = game
        .world()
        .last_event(EventKind::ToolCall)
        .unwrap()
        .unwrap()
        .seq;
    let world = game.world_mut();
    let judgment = world
        .log_event(
            EventKind::Judgment,
            &json!({"reasoning": "The eel-wives drowned the bell secretly"}),
        )
        .unwrap();
    let tick = world
        .log_event(
            EventKind::Tick,
            &json!({"summary": "The eel-wives drowned the bell at night"}),
        )
        .unwrap();
    let known = entries(world).unwrap();
    let last = world.last_seq().unwrap();
    let ash = known[1].id;
    let fact = |excerpt: &str, source: i64| json!({"entry": ash, "facts": [{"text": "Hides a ledger.", "excerpt": excerpt, "source": source}]});
    let bad = [
        // Missing events.
        fact("the bell rope", last + 50),
        // Not narration: the player's own words, a tool call, a judge's
        // reasoning, an off-screen tick.
        fact("listen to the monk", input),
        fact("A gaunt monk ties", tool_call),
        fact("drowned the bell secretly", judgment),
        fact("drowned the bell at", tick),
        // Narration that doesn't say it; the hidden bible secret.
        fact("Ash hides the ledger", narration),
        fact("The abbot drowned the bell to hide the ledger", narration),
        // Too short to pin anything down.
        fact("bell", narration),
        // No citation at all.
        json!({"entry": ash, "facts": [{"text": "Hides a ledger.", "excerpt": "the bell rope"}]}),
        // Not an entry.
        json!({"entry": input, "facts": [{"text": "x", "excerpt": "A gaunt monk ties", "source": narration}]}),
    ];
    for d in bad {
        let d: Discovery = serde_json::from_value(d).unwrap();
        assert!(learn(world, &d, None).unwrap().is_err(), "{d:?}");
    }
    assert_eq!(world.last_seq().unwrap(), last, "nothing was written");
    assert_eq!(entries(world).unwrap(), known, "what was known is intact");

    // The director gets the same refusals at once, for earlier sources.
    learning_turn(
        &mut game,
        &client,
        &[
            fact("drowned the bell at", tick),
            fact("listen to the monk", input),
        ],
        "You wait.",
    );
    let results = tool_results(&client, 3);
    assert!(results.iter().all(|(_, err)| *err), "{results:?}");
    assert!(results[0].0.contains("not narration"), "{results:?}");
    assert_eq!(entries(game.world()).unwrap(), known);

    // A cited earlier narration with the right words is accepted.
    let ok: Discovery = serde_json::from_value(fact("ties off the bell rope", narration)).unwrap();
    let world = game.world_mut();
    assert_eq!(learn(world, &ok, None).unwrap(), Ok(ash));
    assert_eq!(
        entry(world, ash)
            .unwrap()
            .unwrap()
            .facts
            .last()
            .unwrap()
            .sources,
        [Source {
            event: narration,
            excerpt: Some("ties off the bell rope".into())
        }]
    );
}

#[test]
fn failed_or_incomplete_turns_commit_no_discovery() {
    // The turn fails after staging: nothing is learned.
    let (mut game, client) = begun();
    for l in ash_and_the_bell() {
        client.push(learn_call(l));
    }
    assert!(game.turn(&client, "listen", &mut |_| {}).is_err());
    assert_eq!(names(game.world()), ["Drowned Chapel"]);

    // Cut off at max tokens: the narration holds the excerpts, but the turn
    // is incomplete.
    let (mut game, client) = begun();
    for l in ash_and_the_bell() {
        client.push(learn_call(l));
    }
    let mut cut = Response::canned_text(ASH_NARRATION);
    cut.stop_reason = Some(StopReason::MaxTokens);
    client.push(cut);
    game.turn(&client, "listen", &mut |_| {}).unwrap();
    assert_eq!(names(game.world()), ["Drowned Chapel"]);

    // A turn that narrates nothing.
    let (mut game, client) = begun();
    learning_turn(&mut game, &client, &ash_and_the_bell(), "");
    assert_eq!(names(game.world()), ["Drowned Chapel"]);

    // Narration that leaves out one discovery's words: only the supported
    // one is kept, and earlier knowledge is untouched.
    let (mut game, client) = begun();
    learning_turn(&mut game, &client, &ash_and_the_bell()[..1], ASH_NARRATION);
    let ash = one(game.world(), "Brother Ash");
    learning_turn(
        &mut game,
        &client,
        &[
            json!({"entry": ash.id, "facts": [
                {"text": "Has a limp.", "excerpt": "Ash limps toward the door"}]}),
            json!({"name": "The ledger", "category": "item", "facts": [
                {"text": "Bound in eel skin.", "excerpt": "a ledger bound in eel skin"}]}),
        ],
        "On the lectern lies a ledger bound in eel skin.",
    );
    assert_eq!(one(game.world(), "Brother Ash"), ash);
    assert_eq!(
        names(game.world()),
        ["Drowned Chapel", "Brother Ash", "The ledger"]
    );
}

#[test]
fn every_category_lists_searches_and_reads_through_the_shared_command() {
    let (mut game, client) = begun();
    let narration = "Brother Ash bows. The Salt Tower looms. The Eel-wives keep the old tithes. \
                     You pocket a brass key. The fen remembers the flood. Bells ring when the \
                     tide turns.";
    let learns: Vec<Value> = [
        ("Brother Ash", "person", "Brother Ash bows"),
        ("Salt Tower", "place", "The Salt Tower looms"),
        ("Eel-wives", "faction", "keep the old tithes"),
        ("Brass key", "item", "pocket a brass key"),
        ("The flood", "lore", "fen remembers the flood"),
        ("Tide bells", "mechanic", "Bells ring when the tide turns"),
    ]
    .iter()
    .map(|(name, category, excerpt)| {
        json!({"name": name, "category": category, "facts": [
            {"text": format!("{excerpt}."), "excerpt": excerpt}]})
    })
    .collect();
    learning_turn(&mut game, &client, &learns, narration);
    let calls = client.requests().len();
    let world = game.world();
    let overview = compendium(world, "");
    for (heading, word, name) in [
        ("People", "people", "Brother Ash"),
        ("Places", "places", "Salt Tower"),
        ("Factions", "factions", "Eel-wives"),
        ("Items", "items", "Brass key"),
        ("Lore", "lore", "The flood"),
        ("Mechanics", "mechanics", "Tide bells"),
    ] {
        assert!(overview.contains(heading), "{heading}: {overview}");
        let section = compendium(world, word);
        assert!(section.starts_with(heading), "{section}");
        assert!(section.contains(name), "{section}");
        assert!(compendium(world, name).contains(&format!("{name} · ")));
    }
    assert!(compendium(world, "search tithes").contains("Eel-wives (faction)"));
    assert!(compendium(world, "search tide").contains("Tide bells (mechanic)"));
    // Nothing unlearned: the bible's NPC description and secret, room
    // contents and props.
    for hidden in [
        "lies kindly",
        "abbot",
        "ledger",
        "slate",
        "candle",
        "Saltmarsh",
    ] {
        let shown = [
            compendium(world, ""),
            compendium(world, "people"),
            compendium(world, "Brother Ash"),
            compendium(world, "Eel-wives"),
            compendium(world, &format!("search {hidden}")),
        ]
        .join("\n")
        .replace(&format!("{hidden:?}"), "");
        assert!(!shown.contains(hidden), "{hidden} leaked:\n{shown}");
    }
    assert_eq!(client.requests().len(), calls, "reading made no model call");
}

#[test]
fn learned_discoveries_survive_reopen_replay_and_fork() {
    use crate::branches::tests::{saved_game, temp_dir};
    let dir = temp_dir("knowledge-learned");
    let path = saved_game(&dir, "mill.db");
    let mut game = Game::new(World::open(&path).unwrap());
    let client = ScriptedClient::new([]);
    learning_turn(&mut game, &client, &ash_and_the_bell(), ASH_NARRATION);
    let want = entries(game.world()).unwrap();
    assert_eq!(want.len(), 3);
    let last = game.world().last_seq().unwrap();
    drop(game);

    let reopened = World::open(&path).unwrap();
    assert_eq!(entries(&reopened).unwrap(), want);
    assert_eq!(
        entries(&reopened.replay_in_memory().unwrap()).unwrap(),
        want
    );
    assert_eq!(
        entries(&reopened.fork(last, dir.join("fork.db")).unwrap()).unwrap(),
        want
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn director_patches_and_ticks_cannot_write_knowledge() {
    let (mut game, client) = begun();
    learning_turn(&mut game, &client, &ash_and_the_bell(), ASH_NARRATION);
    let known = entries(game.world()).unwrap();
    let room = super::player_room(&game);
    let narration = known[1].first_seen;
    let forged = json!({"category": "person", "summary": "forged", "summary_source": narration,
                        "first_seen": narration, "last_seen": narration, "encounters": 0, "facts": []});
    for op in [
        json!({"op": "create", "name": "Forged", "kind": "knowledge", "props": {"_entry": forged}}),
        json!({"op": "set_prop", "entity": room, "key": "_entry", "value": forged}),
        json!({"op": "set_prop", "entity": known[1].id, "key": "_entry", "value": forged}),
    ] {
        let patch: Patch = serde_json::from_value(json!({ "ops": [op] })).unwrap();
        assert!(
            matches!(
                game.world_mut().apply_patch(&patch),
                Err(ApplyError::Rejected(_))
            ),
            "{op}"
        );
    }
    // Nor through the director's patch tool.
    client.push(Response::canned_tool_use(
        "p",
        "apply_patch",
        json!({"ops": [{"op": "create", "name": "Forged", "kind": "knowledge", "location": room,
                        "props": {"_entry": forged}}]}),
    ));
    client.push(Response::canned_text("You wait."));
    game.turn(&client, "wait", &mut |_| {}).unwrap();
    assert_eq!(entries(game.world()).unwrap(), known);

    // A tick can't learn: the tool isn't offered there.
    client.push(learn_call(ash_and_the_bell()[0].clone()));
    client.push(Response::canned_text("Brother Ash bows off-screen."));
    game.tick(&client).unwrap();
    let tick_request = client.requests().last().cloned().unwrap();
    assert!(tick_request.tools.iter().all(|t| t.name != "learn"));
    match tick_request
        .messages
        .last()
        .unwrap()
        .content
        .first()
        .unwrap()
    {
        ContentBlock::ToolResult {
            content, is_error, ..
        } => assert!(*is_error && content.contains("no tool named"), "{content}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(entries(game.world()).unwrap(), known);
}

#[test]
fn only_the_unnarrated_fact_is_dropped() {
    let (mut game, client) = begun();
    learning_turn(
        &mut game,
        &client,
        &[
            json!({"name": "Brother Ash", "category": "person", "facts": [
                {"text": "A gaunt monk.", "excerpt": "A gaunt monk ties off"},
                {"text": "Limps.", "excerpt": "Ash limps toward the door"}
            ]}),
        ],
        ASH_NARRATION,
    );
    let ash = one(game.world(), "Brother Ash");
    assert_eq!(ash.facts.len(), 1);
    assert_eq!(ash.facts[0].text, "A gaunt monk.");
}

#[test]
fn a_failed_turns_staged_calls_do_not_block_the_next_turn() {
    let (mut game, client) = begun();
    client.push(learn_call(ash_and_the_bell()[0].clone()));
    assert!(game.turn(&client, "listen", &mut |_| {}).is_err());
    let calls = client.requests().len();
    learning_turn(&mut game, &client, &ash_and_the_bell()[..1], ASH_NARRATION);
    let results = tool_results(&client, client.requests().len() - calls);
    assert!(!results[0].1, "{results:?}");
    assert_eq!(names(game.world()), ["Drowned Chapel", "Brother Ash"]);
}

#[test]
fn two_calls_about_one_entity_in_a_turn_are_refused_when_staged() {
    let (mut game, client) = begun();
    let altar = game
        .world()
        .contents(Some(super::player_room(&game)))
        .unwrap()
        .into_iter()
        .find(|&id| game.world().entity(id).unwrap().unwrap().name == "altar")
        .unwrap();
    let calls = client.requests().len();
    learning_turn(
        &mut game,
        &client,
        &[
            json!({"name": "Old altar", "category": "item", "subject": altar, "facts": [
                {"text": "Dark stone.", "excerpt": "the altar is dark stone"}]}),
            json!({"name": "Stone table", "category": "item", "subject": altar, "facts": [
                {"text": "Wet.", "excerpt": "the altar is dark stone"}]}),
        ],
        "You see the altar is dark stone.",
    );
    let results = tool_results(&client, client.requests().len() - calls);
    assert!(!results[0].1, "{results:?}");
    assert!(
        results[1].1 && results[1].0.contains("already records"),
        "{results:?}"
    );
    assert_eq!(names(game.world()), ["Drowned Chapel", "Old altar"]);
}

#[test]
fn learning_about_the_room_entered_this_turn_joins_its_encounter() {
    let (mut game, client) = begun();
    let chapel = super::player_room(&game);
    // The tower is the stub room behind the chapel's north exit.
    let tower = game
        .world()
        .relations_from(chapel, Some(crate::world::EXIT_REL))
        .unwrap()[0]
        .object;
    client.push(learn_call(
        json!({"name": "Bell Tower", "category": "place", "subject": tower,
        "facts": [{"text": "Its bell is gone.", "excerpt": "the bell is gone"}]}),
    ));
    client.push(tower_room());
    client.push(Response::canned_text("You climb in; the bell is gone."));
    game.turn(&client, "go north", &mut |_| {}).unwrap();
    let world = game.world();
    assert_eq!(names(world), ["Drowned Chapel", "Bell Tower"]);
    let entry = one(world, "Bell Tower");
    let narration = world.last_event(EventKind::Narration).unwrap().unwrap().seq;
    assert_eq!(entry.encounters, 1);
    assert_eq!(entry.first_seen, narration);
    assert_eq!(entry.summary, "A leaning tower.");
    assert!(entry.facts.iter().any(|f| f.text == "Its bell is gone."));
}

#[test]
fn an_excerpt_must_match_whole_words_and_ignores_emphasis() {
    let (mut game, _client) = begun();
    let world = game.world_mut();
    let seq = world
        .log_event(
            EventKind::Narration,
            &json!({"text": "The monk *whispers* the old name."}),
        )
        .unwrap();
    let ash: Discovery = serde_json::from_value(json!({"name": "Ash", "category": "person",
        "facts": [{"text": "Whispers.", "excerpt": "whispers the old name", "source": seq}]}))
    .unwrap();
    assert!(learn(world, &ash, None).unwrap().is_ok());
    let partial: Discovery = serde_json::from_value(json!({"name": "Mon", "category": "person",
        "facts": [{"text": "x", "excerpt": "nk whispers the", "source": seq}]}))
    .unwrap();
    assert!(learn(world, &partial, None).unwrap().is_err());
}

#[test]
fn search_finds_who_made_a_claim() {
    let (mut game, client) = begun();
    learning_turn(&mut game, &client, &ash_and_the_bell(), ASH_NARRATION);
    let hits = compendium(game.world(), "search ash");
    assert!(hits.contains("The drowned bell (lore)"), "{hits}");
}
