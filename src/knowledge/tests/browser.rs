use super::*;
use crate::knowledge::browser::query;

#[test]
fn shared_browser_filters_all_player_rows_links_and_sources_before_display() {
    let (mut game, client) = in_tower();
    let room = player_room(&game);
    game.world_mut()
        .apply_patch(
            &serde_json::from_value(json!({"ops": [
                {"op": "create", "name": "Nessa", "kind": "person", "location": room,
                 "props": {"acts": true, "secret": "abbotkiller"}}
            ]}))
            .unwrap(),
        )
        .unwrap();
    client.push(Response::canned_text("Nessa waits by the stairs."));
    game.turn(&client, "look", &mut |_| {}).unwrap();
    let world = game.world();
    let before = world.last_seq().unwrap();
    let calls = client.requests().len();
    let person = place(world, "Nessa");
    let detail = query(world, Mode::Player, &format!("#{}", person.id)).unwrap();
    let known_place = detail
        .links
        .iter()
        .find(|l| l.arg == format!("#{}", person.seen_at.unwrap()))
        .unwrap();
    assert!(
        query(world, Mode::Player, &known_place.arg)
            .unwrap()
            .text
            .contains("Bell Tower")
    );
    let source = detail
        .links
        .iter()
        .find(|l| l.arg.starts_with("source #"))
        .unwrap();
    let narration = query(world, Mode::Player, &source.arg).unwrap();
    assert!(narration.text.contains("Nessa waits by the stairs."));
    assert_eq!(narration.text, compendium(world, &source.arg));
    assert!(compendium(world, "source #1").contains("No learned entry cites"));
    let mut pages = vec![query(world, Mode::Player, "").unwrap(), detail];
    for c in Category::ALL {
        pages.push(query(world, Mode::Player, c.heading()).unwrap());
    }
    pages.push(query(world, Mode::Player, "search abbotkiller").unwrap());
    pages.push(query(world, Mode::Player, "search Nessa").unwrap());
    let all = format!("{pages:?}");
    for hidden in ["abbotkiller", "Brother Ash", "Eel-wives"] {
        // The user's search may repeat its query, but never yields hidden data.
        if hidden == "abbotkiller" {
            assert!(pages[pages.len() - 2].links.is_empty());
        } else {
            assert!(!all.contains(hidden), "{all}");
        }
    }
    assert!(pages.last().unwrap().text.contains("Nessa"));
    assert_eq!(world.last_seq().unwrap(), before);
    assert_eq!(client.requests().len(), calls);
}

#[test]
fn world_browser_uses_stored_projection_with_exact_keys_and_related_links() {
    let (game, client) = begun();
    let world = game.world();
    let before = world.last_seq().unwrap();
    let calls = client.requests().len();
    let people = query(world, Mode::World, "people").unwrap();
    let plan = query(world, Mode::World, &people.links[0].arg).unwrap();
    assert!(plan.text.contains("bible plan"));
    let hits = query(world, Mode::World, "search ledger").unwrap();
    assert!(
        query(world, Mode::World, &hits.links[0].arg)
            .unwrap()
            .text
            .contains("Hidden bible reference")
    );
    let items = query(world, Mode::World, "items").unwrap();
    let candle = items
        .links
        .iter()
        .find(|l| {
            query(world, Mode::World, &l.arg)
                .unwrap()
                .text
                .starts_with("tallow candle")
        })
        .unwrap();
    let candle = query(world, Mode::World, &candle.arg).unwrap();
    assert!(candle.links.iter().any(|l| {
        query(world, Mode::World, &l.arg)
            .unwrap()
            .text
            .starts_with("altar")
    }));
    assert_eq!(world.last_seq().unwrap(), before);
    assert_eq!(client.requests().len(), calls);
}
