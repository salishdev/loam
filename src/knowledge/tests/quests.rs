use super::*;
use crate::{pressure, quest};

pub(crate) fn populate(world: &mut World) -> Vec<i64> {
    let mut ids = Vec::new();
    for (i, status) in ["open", "active", "complete", "failed"].iter().enumerate() {
        let id = quest::spawn(world, &format!("Private title {i}"), "Private hook", &[]).unwrap();
        let mut state = world.entity(id).unwrap().unwrap().props["_quest"].clone();
        state["status"] = json!(status);
        if i > 0 {
            state["goal"] = json!("Private goal");
        }
        if i > 1 {
            state["resolution"] = json!("Private judge reason: secret betrayal");
        }
        state["developments"] = json!(["Private development"]);
        world
            .apply_engine_patch(&Patch::new(vec![Op::SetProp {
                entity: id.into(),
                key: "_quest".into(),
                value: state,
            }]))
            .unwrap();
        let source = world
            .log_event(
                EventKind::Narration,
                &json!({"text": "Help bring the bell home. The keeper needs it before nightfall."}),
            )
            .unwrap();
        let d: Discovery = serde_json::from_value(json!({"name": format!("Bring the bell home {i}"), "category": "lore", "subject": id,
            "facts": [{"text": "Goal: Bring the bell home.", "excerpt": "bring the bell home", "source": source},
                      {"text": "Stakes: The keeper needs it before nightfall.", "excerpt": "The keeper needs it before nightfall", "source": source}]})).unwrap();
        learn(world, &d, None).unwrap().unwrap();
        ids.push(id);
    }
    pressure::start(
        world,
        "Secret flood",
        "Hidden village destruction",
        77,
        &[ids[1]],
    )
    .unwrap();
    ids
}

fn report(w: &World, arg: &str) -> String {
    commands::report(w, ":quests", arg).unwrap().unwrap()
}

#[test]
fn ongoing_history_details_and_search_use_only_the_selected_projection() {
    let mut w = World::open_in_memory().unwrap();
    assert!(report(&w, "").contains("No quests found"));
    let ids = populate(&mut w);
    let hidden = quest::spawn(&mut w, "Unlearned quest", "Hidden hook", &[]).unwrap();
    let last = w.last_seq().unwrap();
    let ongoing = report(&w, "ongoing");
    let history = report(&w, "history");
    for (i, id) in ids.iter().enumerate() {
        assert_eq!(ongoing.contains(&format!("Bring the bell home {i}")), i < 2);
        assert_eq!(
            history.contains(&format!("Bring the bell home {i}")),
            i >= 2
        );
        let text = report(&w, &format!("#{id}"));
        assert!(text.contains("Goal: Bring the bell home."));
        assert!(text.contains("before nightfall"));
        assert_eq!(text.contains("Outcome: Quest complete."), i == 2);
        assert_eq!(text.contains("Outcome: Quest failed."), i == 3);
        assert!(!text.contains("Private"), "{text}");
    }
    for text in [
        ongoing,
        history,
        report(&w, "search Private"),
        report(&w, "search Secret"),
        report(&w, "search destruction"),
        report(&w, &format!("#{hidden}")),
    ] {
        for secret in ["Private", "Unlearned", "Hidden", "Secret flood", "77 min"] {
            assert!(!text.contains(secret), "{text}");
        }
    }
    assert!(report(&w, "Bring the bell home 1").contains("Goal: Bring the bell home"));
    assert!(report(&w, "search nightfall").contains("Bring the bell home 3"));
    assert!(report(&w, "world ongoing").contains("Unlearned quest"));
    let world = report(&w, &format!("world #{}", ids[1]));
    for text in [
        "Private hook",
        "Private goal",
        "Private development",
        "Secret flood",
        "Hidden village destruction",
        "09:17",
    ] {
        assert!(world.contains(text), "{world}");
    }
    assert!(report(&w, &format!("world #{}", ids[2])).contains("Private judge reason"));
    assert_eq!(w.last_seq().unwrap(), last, "browsing wrote nothing");
}

#[test]
fn only_narrated_deadlines_and_explicit_known_relationships_are_shown() {
    let (mut game, client) = begun();
    let ids = populate(game.world_mut());
    let world = game.world_mut();
    let known = entries(world)
        .unwrap()
        .into_iter()
        .find(|e| e.category == Category::Place)
        .unwrap()
        .id;
    let room = world.entities_of_kind(crate::world::ROOM_KIND).unwrap()[0];
    quest::escalate(world, ids[1], "Private relationship", &[room]).unwrap();
    assert!(!report(world, &format!("#{}", ids[1])).contains("Drowned Chapel"));
    let account = about(world, Category::Lore, ids[1]).unwrap().unwrap().0;
    let fact = |related| json!({"entry": account, "facts": [{"text": "Deadline: day 1, 01:17 at the chapel.", "excerpt": "Return at day 1, 01:17", "related": related}]});
    let bad: Discovery = serde_json::from_value(fact(vec![ids[0]])).unwrap();
    assert!(
        check(world, &bad, None, &[]).unwrap().is_err(),
        "raw ids aren't links"
    );
    assert!(!report(world, &format!("#{}", ids[1])).contains("01:17"));
    let calls = client.requests().len();
    super::learned::learning_turn(
        &mut game,
        &client,
        &[fact(vec![known])],
        "Return at day 1, 01:17 to the chapel.",
    );
    let page = crate::knowledge::quests::query(game.world(), Mode::Player, &format!("#{}", ids[1]))
        .unwrap();
    assert!(page.text.contains("Deadline: day 1, 01:17"));
    assert!(
        page.links
            .iter()
            .any(|l| l.cmd == ":compendium" && l.arg == format!("#{known}"))
    );
    assert!(page.text.contains("Drowned Chapel"));
    let after = client.requests().len();
    assert!(after > calls);
    for arg in ["", "world history", "player ongoing", "search chapel"] {
        report(game.world(), arg);
    }
    assert_eq!(client.requests().len(), after);
}

#[test]
fn unsupported_quest_learning_does_not_commit_and_ticks_cannot_teach_it() {
    let (mut game, client) = begun();
    let id = quest::spawn(game.world_mut(), "Private", "Hidden", &[]).unwrap();
    let d = json!({"name": "The bell", "category": "lore", "subject": id,
        "facts": [{"text": "Return before dawn.", "excerpt": "Return before the dawn"}]});
    super::learned::learning_turn(
        &mut game,
        &client,
        std::slice::from_ref(&d),
        "You wait in silence.",
    );
    assert!(!report(game.world(), "").contains("The bell"));
    client.push(Response::canned_tool_use("l", "learn", d));
    assert!(game.turn(&client, "listen", &mut |_| {}).is_err());
    assert!(!report(game.world(), "").contains("The bell"));
    client.push(Response::canned_text("The world moves on."));
    game.tick(&client).unwrap();
    assert!(!report(game.world(), "").contains("The bell"));
}

#[test]
fn quest_knowledge_and_filters_survive_resume_replay_and_fork() {
    let dir = crate::branches::tests::temp_dir("quest-browser");
    let path = dir.join("quests.db");
    let mut w = World::open(&path).unwrap();
    let ids = populate(&mut w);
    let want = report(&w, &format!("#{}", ids[1]));
    let last = w.last_seq().unwrap();
    let replay = w.replay_in_memory().unwrap();
    assert_eq!(report(&replay, &format!("#{}", ids[1])), want);
    let fork = w.fork(last, dir.join("fork.db")).unwrap();
    assert_eq!(report(&fork, &format!("#{}", ids[1])), want);
    drop(fork);
    drop(w);
    let w = World::open(&path).unwrap();
    assert_eq!(report(&w, &format!("#{}", ids[1])), want);
    assert!(report(&w, "history").contains("Bring the bell home 3"));
    drop(w);
    std::fs::remove_dir_all(dir).unwrap();
}
