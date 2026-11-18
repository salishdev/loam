use super::*;
use crate::game::tests::begun;

#[test]
fn every_topic_has_a_name_that_parses_and_a_short_text() {
    assert_eq!(Topic::ALL.len(), 10);
    for t in Topic::ALL {
        assert_eq!(Topic::parse(t.name()), Some(t));
        let sentences = t.text().matches(". ").count() + 1;
        assert!((1..=2).contains(&sentences), "{}: {}", t.name(), t.text());
        // A list line fits an 80-column terminal with its margin.
        assert!(t.summary().chars().count() + 16 <= 76, "{}", t.summary());
    }
    assert_eq!(Topic::parse(" Fast-Path "), Some(Topic::FastPath));
    assert_eq!(Topic::parse("dragons"), None);
    assert!(Topic::Refusal.text().contains("the engine can't"));
}

#[test]
fn explain_lists_topics_and_shows_one() {
    let list = explain("");
    assert!(list.starts_with("Topics"));
    for t in Topic::ALL {
        assert!(list.contains(&format!("\n  {}", t.name())), "{list}");
    }
    assert_eq!(explain("cache"), Topic::Cache.line());
    assert!(explain("cache").starts_with("cache: Every call"));
    let unknown = explain("dragons");
    assert!(unknown.starts_with("No topic dragons.") && unknown.contains("reroll-split"));
}

#[test]
fn a_turns_xray_names_its_concepts() {
    let (game, seq) = crate::xray::tests::played();
    let x = crate::xray::turn(game.world(), seq).unwrap().unwrap();
    assert_eq!(
        turn_topics(&x),
        [
            Topic::Director,
            Topic::Cache,
            Topic::Cost,
            Topic::Refusal,
            Topic::Rule
        ]
    );
}

#[test]
fn a_reroll_split_is_named_only_when_variants_disagree() {
    let mut s = Summary {
        routes: vec![("director".into(), vec![1, 2])],
        facts: vec![("door.broken".into(), vec![1, 2])],
        done: 2,
        ..Summary::default()
    };
    assert!(reroll_topics(&s).is_empty());
    s.facts.push(("rule minted".into(), vec![2]));
    assert_eq!(reroll_topics(&s), [Topic::RerollSplit]);
    s.facts.pop();
    s.routes = vec![("director".into(), vec![1]), ("fast path".into(), vec![2])];
    s.cost_usd = 0.01;
    assert_eq!(reroll_topics(&s), [Topic::RerollSplit, Topic::Cost]);
}

#[test]
fn each_topic_is_explained_once_per_save() {
    let (game, _) = begun();
    let world = game.world();
    let mut notes = Notes::load(world);
    assert_eq!(
        notes.fresh([Topic::Cache, Topic::Rule, Topic::Cache]),
        [Topic::Cache, Topic::Rule]
    );
    assert!(notes.fresh([Topic::Rule]).is_empty());
    world.mark_explained(&[Topic::Cache, Topic::Rule]).unwrap();
    world.mark_explained(&[Topic::Rule]).unwrap();
    // A new session over the same save remembers them.
    let mut again = Notes::load(world);
    assert_eq!(again.fresh([Topic::Rule, Topic::Brain]), [Topic::Brain]);
    assert_eq!(
        world.explained().unwrap(),
        BTreeSet::from([Topic::Cache, Topic::Rule])
    );
}

#[test]
fn a_branch_keeps_what_its_parent_explained() {
    use crate::branches::tests::{saved_game, temp_dir};
    let dir = temp_dir("explain-fork");
    let main = World::open(saved_game(&dir, "mill.db")).unwrap();
    main.mark_explained(&[Topic::Branch]).unwrap();
    let branch = main.fork(1, dir.join("b.db")).unwrap();
    assert_eq!(branch.explained().unwrap(), BTreeSet::from([Topic::Branch]));
    drop((main, branch));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn the_worker_saves_explained_topics() {
    use crate::worker::{Command, Worker};
    let (game, client) = begun();
    let dir = crate::branches::tests::temp_dir("explain-worker");
    let path = dir.join("w.db");
    let saved = game
        .world()
        .fork(game.world().last_seq().unwrap(), &path)
        .unwrap();
    drop((game, saved));
    let worker = Worker::spawn(
        crate::game::Game::new(World::open(&path).unwrap()),
        Box::new(client),
    );
    worker.send(Command::Explained(vec![Topic::Context]));
    worker.shut_down();
    let world = World::open(&path).unwrap();
    assert_eq!(world.explained().unwrap(), BTreeSet::from([Topic::Context]));
    drop(world);
    std::fs::remove_dir_all(&dir).unwrap();
}
