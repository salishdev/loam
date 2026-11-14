use serde_json::{Value, json};

use super::*;
use crate::game::Game;
use crate::llm::{ContentBlock, Model, Response, ScriptedClient};

/// A world with a room holding a well and a bucket.
fn world() -> (World, EntityId, EntityId, EntityId) {
    let mut w = World::open_in_memory().unwrap();
    let a = w
        .apply_patch(&Patch::new(vec![
            Op::Create {
                handle: Some("room".into()),
                name: "Yard".into(),
                kind: "room".into(),
                location: None,
                props: Map::new(),
            },
            Op::Create {
                handle: Some("well".into()),
                name: "well".into(),
                kind: "feature".into(),
                location: Some("room".into()),
                props: Map::new(),
            },
            Op::Create {
                handle: None,
                name: "bucket".into(),
                kind: "item".into(),
                location: Some("room".into()),
                props: Map::new(),
            },
        ]))
        .unwrap();
    (w, a.created[0], a.created[1], a.created[2])
}

fn refused<T: std::fmt::Debug>(r: Result<T>) -> String {
    match r {
        Err(QuestError::Refused(reason)) => reason,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn spawn_then_reveal_records_goal_and_concerns() {
    let (mut w, _, well, bucket) = world();
    let id = spawn(
        &mut w,
        "The dry well",
        "The miller begs for water.",
        &[well],
    )
    .unwrap();
    let q = quest(&w, id).unwrap().unwrap();
    assert_eq!(q.status, Status::Open);
    assert_eq!(q.goal, None);
    assert_eq!(q.concerns, vec![well]);

    let q = reveal_goal(
        &mut w,
        id,
        "Fill the bucket from the well.",
        &[bucket, well],
    )
    .unwrap();
    assert_eq!(q.status, Status::Active);
    assert_eq!(q.goal.as_deref(), Some("Fill the bucket from the well."));
    assert_eq!(q.concerns, vec![well, bucket], "merged without duplicates");

    // The goal is fixed once recorded.
    let reason = refused(reveal_goal(&mut w, id, "Something else.", &[well]));
    assert!(reason.contains("Goals never change"), "{reason}");

    let q = escalate(&mut w, id, "The miller's child falls ill.", &[]).unwrap();
    assert_eq!(q.developments, vec!["The miller's child falls ill."]);
    assert_eq!(q.goal.as_deref(), Some("Fill the bucket from the well."));
}

#[test]
fn bad_concerns_are_refused() {
    let (mut w, _, well, _) = world();
    assert!(refused(spawn(&mut w, "x", "y", &[999])).contains("does not exist"));
    let id = spawn(&mut w, "x", "y", &[]).unwrap();
    assert!(refused(reveal_goal(&mut w, id, "goal", &[])).contains("at least one"));
    assert!(refused(reveal_goal(&mut w, id, "goal", &[id])).contains("is a quest"));
    assert!(refused(reveal_goal(&mut w, well, "goal", &[well])).contains("not a quest"));
    assert!(refused(spawn(&mut w, " ", "y", &[])).contains("title"));
}

#[test]
fn touched_covers_every_op_and_resolves_handles() {
    let (mut w, room, well, bucket) = world();
    let start = w.last_seq().unwrap();
    assert!(touched_since(&w, start).unwrap().is_empty());
    let a = w
        .apply_patch(&Patch::new(vec![
            Op::Create {
                handle: Some("rope".into()),
                name: "rope".into(),
                kind: "item".into(),
                location: None,
                props: Map::new(),
            },
            Op::Move {
                entity: "rope".into(),
                to: Some(well.into()),
            },
            Op::SetProp {
                entity: bucket.into(),
                key: "full".into(),
                value: json!(true),
            },
        ]))
        .unwrap();
    let rope = a.created[0];
    // A rejected patch touches nothing.
    w.apply_patch(&Patch::new(vec![Op::Destroy {
        entity: room.into(),
    }]))
    .unwrap_err();
    let touched: Vec<_> = touched_since(&w, start).unwrap().into_iter().collect();
    let mut want = vec![rope, well, bucket];
    want.sort();
    assert_eq!(touched, want);
}

#[test]
fn director_patches_cannot_touch_quests() {
    let (game, client) = begun();
    let mut game = game;
    let well = find(&game, "well");
    let id = spawn(game.world_mut(), "The dry well", "hook", &[well]).unwrap();
    client.push(Response::canned_tool_use(
        "p",
        "apply_patch",
        json!({"ops": [{"op": "set_prop", "entity": id, "key": "goal", "value": "x"}]}),
    ));
    client.push(Response::canned_text("Nothing happens."));
    game.turn(&client, "rewrite the quest", &mut |_| {})
        .unwrap();
    let (content, is_error) = last_tool_result(&client);
    assert!(is_error);
    assert!(content.contains("is a quest"), "{content}");
}

// ---- the full loop through Game -------------------------------------------

fn begun() -> (Game, ScriptedClient) {
    let client = ScriptedClient::new([
        crate::stats::with_sample_sheet(Response::canned_tool_use(
            "b1",
            "write_bible",
            json!({
                "tone": "Dusty.",
                "regions": [{"name": "Mill", "description": "A dry mill town."}],
                "factions": [{"name": "Millers", "description": "Need water."}],
                "npcs": [{"name": "Old Hal", "description": "The miller."}],
                "secrets": [{"name": "Spring", "description": "Under the well."}]
            }),
        )),
        Response::canned_text(""),
        Response::canned_tool_use(
            "r1",
            "create_room",
            json!({
                "name": "Mill Yard",
                "region": "Mill",
                "description": "Cracked earth.",
                "exits": {},
                "contents": [
                    {"name": "well", "kind": "feature"},
                    {"name": "bucket", "kind": "item"},
                    {"name": "millstone", "kind": "feature"}
                ]
            }),
        ),
        Response::canned_text("You stand in a dry yard."),
    ]);
    let mut game = Game::new(World::open_in_memory().unwrap());
    game.create_bible(&client, "A drought.").unwrap();
    game.begin(&client, &mut |_| {}).unwrap();
    (game, client)
}

fn find(game: &Game, name: &str) -> EntityId {
    let w = game.world();
    ["feature", "item"]
        .iter()
        .flat_map(|k| w.entities_of_kind(k).unwrap())
        .find(|id| w.entity(*id).unwrap().unwrap().name == name)
        .unwrap()
}

fn last_tool_result(client: &ScriptedClient) -> (String, bool) {
    let req = client
        .requests()
        .into_iter()
        .rev()
        .find(|r| r.model == Model::Sonnet)
        .unwrap();
    match req.messages.last().unwrap().content.first().unwrap() {
        ContentBlock::ToolResult {
            content, is_error, ..
        } => (content.clone(), *is_error),
        other => panic!("expected a tool result, got {other:?}"),
    }
}

fn judgment_events(game: &Game) -> Vec<Value> {
    game.world()
        .events()
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EventKind::Judgment)
        .map(|e| e.payload)
        .collect()
}

/// Spawn and reveal a quest through director tool calls; returns its id.
fn spawn_and_reveal(game: &mut Game, client: &ScriptedClient) -> EntityId {
    let well = find(game, "well");
    let bucket = find(game, "bucket");
    client.push(Response::canned_tool_use(
        "q1",
        "spawn_quest",
        json!({"title": "Water for Hal", "hook": "Old Hal begs for water.", "concerns": [well]}),
    ));
    client.push(Response::canned_text("Hal looks at you, parched."));
    let out = game.turn(client, "talk to hal", &mut |_| {}).unwrap();
    assert!(out.judgments.is_empty(), "an open quest isn't judged");
    let id = game.quests().unwrap()[0].id;
    client.push(Response::canned_tool_use(
        "q2",
        "reveal_goal",
        json!({"quest": id, "goal": "Hal holds a full bucket of water.", "concerns": [bucket]}),
    ));
    client.push(Response::canned_text("Hal points at the bucket."));
    game.turn(client, "ask what he needs", &mut |_| {}).unwrap();
    // Recording the goal touched only the quest itself: no judge call.
    assert!(judgment_events(game).is_empty());
    id
}

#[test]
fn quest_is_spawned_recorded_and_judged_complete() {
    let (mut game, client) = begun();
    let id = spawn_and_reveal(&mut game, &client);
    let bucket = find(&game, "bucket");
    let quest_view = game.quests().unwrap();
    assert_eq!(quest_view[0].status, Status::Active);
    assert_eq!(quest_view[0].concerns.len(), 2);

    // A turn whose patch touches the bucket triggers the judge.
    client.push(Response::canned_tool_use(
        "p1",
        "apply_patch",
        json!({"ops": [{"op": "set_prop", "entity": bucket, "key": "full", "value": true}]}),
    ));
    client.push(Response::canned_text("You haul up cold water."));
    client.push(Response::canned_tool_use(
        "j1",
        JUDGE_TOOL,
        json!({"verdict": "ongoing", "reason": "The bucket is full but Hal doesn't hold it."}),
    ));
    let out = game.turn(&client, "fill the bucket", &mut |_| {}).unwrap();
    assert_eq!(out.judgments.len(), 1);
    assert_eq!(out.judgments[0].verdict, Verdict::Ongoing);

    let judge_req = client.requests().last().cloned().unwrap();
    assert_eq!(judge_req.model, Model::Haiku);
    assert_eq!(judge_req.tools[0].name, JUDGE_TOOL);
    let prompt = match &judge_req.messages[0].content[0] {
        ContentBlock::Text { text } => text.clone(),
        other => panic!("{other:?}"),
    };
    assert!(prompt.contains("Recorded goal: Hal holds a full bucket of water."));
    assert!(prompt.contains("bucket [item]"), "{prompt}");
    assert!(prompt.contains("(changed this turn)"));
    assert!(prompt.contains("Player input: fill the bucket"));
    assert!(prompt.contains("You haul up cold water."));

    // A turn that touches nothing the quest concerns makes no judge call.
    let millstone = find(&game, "millstone");
    client.push(Response::canned_tool_use(
        "p2",
        "apply_patch",
        json!({"ops": [{"op": "set_prop", "entity": millstone, "key": "turning", "value": true}]}),
    ));
    client.push(Response::canned_text("The stone grinds."));
    let out = game
        .turn(&client, "push the millstone", &mut |_| {})
        .unwrap();
    assert!(out.judgments.is_empty());
    assert_eq!(client.remaining(), 0);
    assert_eq!(judgment_events(&game).len(), 1);

    // Handing over the bucket completes it.
    client.push(Response::canned_tool_use(
        "p3",
        "apply_patch",
        json!({"ops": [{"op": "set_prop", "entity": bucket, "key": "held_by", "value": "Hal"}]}),
    ));
    client.push(Response::canned_text("Hal drinks deep."));
    client.push(Response::canned_tool_use(
        "j2",
        JUDGE_TOOL,
        json!({"verdict": "complete", "reason": "Hal holds the full bucket."}),
    ));
    let out = game
        .turn(&client, "give hal the bucket", &mut |_| {})
        .unwrap();
    assert_eq!(out.judgments[0].verdict, Verdict::Complete);
    let q = quest(game.world(), id).unwrap().unwrap();
    assert_eq!(q.status, Status::Complete);
    assert_eq!(q.resolution.as_deref(), Some("Hal holds the full bucket."));

    // Every judgment is logged.
    let logged = judgment_events(&game);
    assert_eq!(logged.len(), 2);
    assert_eq!(logged[1]["quest"], json!(id));
    assert_eq!(logged[1]["verdict"], "complete");
    assert_eq!(logged[1]["reason"], "Hal holds the full bucket.");
    assert_eq!(logged[1]["touched"], json!([bucket]));
    assert_eq!(logged[1]["model"], Model::Haiku.id());

    // A resolved quest is no longer judged, and can't be escalated.
    client.push(Response::canned_tool_use(
        "p4",
        "apply_patch",
        json!({"ops": [{"op": "set_prop", "entity": bucket, "key": "full", "value": false}]}),
    ));
    client.push(Response::canned_text("Empty again."));
    let out = game.turn(&client, "kick the bucket", &mut |_| {}).unwrap();
    assert!(out.judgments.is_empty());
    assert!(refused(escalate(game.world_mut(), id, "more", &[])).contains("complete"));

    // The director sees it in its slice.
    let slice = crate::game::local_slice(game.world()).unwrap();
    assert!(
        slice.contains(&format!("quest #{id} Water for Hal [complete]")),
        "{slice}"
    );

    // Replay rebuilds the quest state from the log with no LLM.
    let replayed = game.world().replay_in_memory().unwrap();
    assert_eq!(replayed.state().unwrap(), game.world().state().unwrap());
    assert_eq!(
        quest(&replayed, id).unwrap().unwrap().status,
        Status::Complete
    );
}

#[test]
fn judge_can_fail_a_quest_and_slice_shows_active_goal() {
    let (mut game, client) = begun();
    let id = spawn_and_reveal(&mut game, &client);
    let slice = crate::game::local_slice(game.world()).unwrap();
    assert!(
        slice.contains("[active] goal: Hal holds a full bucket of water."),
        "{slice}"
    );
    let bucket = find(&game, "bucket");
    client.push(Response::canned_tool_use(
        "p1",
        "apply_patch",
        json!({"ops": [{"op": "destroy", "entity": bucket}]}),
    ));
    client.push(Response::canned_text("The bucket falls apart."));
    client.push(Response::canned_tool_use(
        "j1",
        JUDGE_TOOL,
        json!({"verdict": "failed", "reason": "The only bucket is gone."}),
    ));
    game.turn(&client, "smash the bucket", &mut |_| {}).unwrap();
    let judge_req = client.requests().last().cloned().unwrap();
    let ContentBlock::Text { text } = &judge_req.messages[0].content[0] else {
        panic!()
    };
    assert!(text.contains(&format!("#{bucket}: destroyed")), "{text}");
    assert_eq!(
        quest(game.world(), id).unwrap().unwrap().status,
        Status::Failed
    );
}

#[test]
fn missing_verdict_is_logged_and_changes_nothing() {
    let (mut game, client) = begun();
    let id = spawn_and_reveal(&mut game, &client);
    let bucket = find(&game, "bucket");
    client.push(Response::canned_tool_use(
        "p1",
        "apply_patch",
        json!({"ops": [{"op": "set_prop", "entity": bucket, "key": "full", "value": true}]}),
    ));
    client.push(Response::canned_text("Water sloshes."));
    client.push(Response::canned_text("I think it's done?"));
    let out = game.turn(&client, "fill it", &mut |_| {}).unwrap();
    assert_eq!(out.judgments[0].verdict, Verdict::Ongoing);
    let logged = judgment_events(&game);
    assert_eq!(logged[0]["error"], "the judge gave no verdict");
    assert_eq!(
        quest(game.world(), id).unwrap().unwrap().status,
        Status::Active
    );

    // A failed judge call doesn't fail the turn either.
    client.push(Response::canned_tool_use(
        "p2",
        "apply_patch",
        json!({"ops": [{"op": "set_prop", "entity": bucket, "key": "full", "value": false}]}),
    ));
    client.push(Response::canned_text("Spilled."));
    let out = game.turn(&client, "spill it", &mut |_| {}).unwrap();
    assert_eq!(out.judgments[0].verdict, Verdict::Ongoing);
    let logged = judgment_events(&game);
    assert!(
        logged[1]["error"]
            .as_str()
            .unwrap()
            .starts_with("judge call failed"),
        "{}",
        logged[1]
    );
}

#[test]
fn concerns_are_relations_and_destroying_one_leaves_a_valid_quest() {
    let (mut w, _, well, bucket) = world();
    let id = spawn(&mut w, "The dry well", "The miller begs.", &[well]).unwrap();
    reveal_goal(&mut w, id, "Fill the bucket.", &[bucket]).unwrap();

    // No entity id in the quest's JSON: concerns are relations.
    let e = w.entity(id).unwrap().unwrap();
    assert_eq!(e.props.keys().collect::<Vec<_>>(), vec!["_quest"]);
    let rels = w.relations_from(id, Some(CONCERNS_REL)).unwrap();
    assert_eq!(
        rels.iter().map(|r| r.object).collect::<Vec<_>>(),
        vec![well, bucket]
    );

    w.apply_patch(&Patch::new(vec![Op::Destroy {
        entity: bucket.into(),
    }]))
    .unwrap();
    let q = quest(&w, id).unwrap().unwrap();
    assert_eq!(q.concerns, vec![well]);
    assert_eq!(q.status, Status::Active);
    // Still escalates and takes new concerns.
    let q = escalate(&mut w, id, "The well cracks.", &[]).unwrap();
    assert_eq!(q.developments.len(), 1);

    // Replay reproduces it.
    let r = w.replay_in_memory().unwrap();
    assert_eq!(r.state().unwrap(), w.state().unwrap());
}

#[test]
fn director_cannot_touch_concerns() {
    let (mut w, _, well, _) = world();
    let id = spawn(&mut w, "The dry well", "The miller begs.", &[well]).unwrap();
    let err = w
        .apply_patch(&Patch::new(vec![Op::Unrelate {
            subject: id.into(),
            rel: CONCERNS_REL.into(),
            object: well.into(),
        }]))
        .unwrap_err();
    assert!(err.to_string().contains("engine-owned"), "{err}");
}
