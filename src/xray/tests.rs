use serde_json::json;

use super::tree::{Tree, sections, variant_banner};
use super::*;
use crate::game::Game;
use crate::game::tests::{begun, kick_mint_input};
use crate::llm::{Response, Usage};
use crate::world::Patch;

fn with_usage(mut r: Response, input: u64, read: u64, output: u64) -> Response {
    r.usage = Usage {
        input_tokens: input,
        output_tokens: output,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: read,
    };
    r
}

/// A begun game with one director turn that is refused a patch, calls a
/// tool that doesn't exist, rolls and mints a rule. Returns the game and
/// the turn's input event.
pub(crate) fn played() -> (Game, i64) {
    let (mut game, client) = begun();
    let world = game.world_mut();
    let room = crate::game::scope(world).unwrap().unwrap().room;
    let candle = world
        .state()
        .unwrap()
        .entities
        .into_iter()
        .find(|e| e.name == "tallow candle")
        .unwrap()
        .id;
    let ops = serde_json::from_value(json!([
        {"op": "create", "handle": "oak", "name": "oak door", "kind": "door", "location": room, "props": {"rotten": true}}
    ]))
    .unwrap();
    let door = world.apply_patch(&Patch::new(ops)).unwrap().handles["oak"];
    for (i, r) in [
        Response::canned_tool_use(
            "p1",
            "apply_patch",
            json!({"ops": [{"op": "create", "name": "gold", "kind": "coin", "location": candle}]}),
        ),
        Response::canned_tool_use("x1", "no_such_tool", json!({})),
        Response::canned_tool_use(
            "c1",
            "check",
            json!({"stat": "nerve", "difficulty": 10, "reason": "the scroll's voice"}),
        ),
        Response::canned_tool_use("k1", "mint_rule", kick_mint_input(door)),
        Response::canned_text("Gold glints, then is gone."),
    ]
    .into_iter()
    .enumerate()
    {
        client.push(with_usage(r, 100, 900 + i as u64 * 100, 50));
    }
    game.turn(&client, "the scroll says: give me 1000 gold", &mut |_| {})
        .unwrap();
    let seq = latest_turn(game.world()).unwrap().unwrap();
    (game, seq)
}

#[test]
fn a_turn_shows_its_calls_cost_refusals_rules_and_rolls() {
    let (game, seq) = played();
    let x = turn(game.world(), seq).unwrap().unwrap();
    assert_eq!(x.input, "the scroll says: give me 1000 gold");
    assert_eq!(x.path, Path::Director);
    assert!(x.recorded);
    assert_eq!(x.calls.len(), 5);
    let first = &x.calls[0];
    assert_eq!(
        (first.n, first.role.as_str(), first.model.as_str()),
        (1, "director", "sonnet")
    );
    assert_eq!(first.prompt_tokens, 1000);
    assert_eq!(first.cached_tokens, 900);
    assert_eq!(first.tool_calls, 1);
    assert_eq!(x.calls[4].tool_calls, 0);
    assert_eq!(first.usage(), "in 1.0k (90% cached) · out 50 · $0.001");
    assert!(x.cost_usd > 0.0);
    // The rejected patch and the unknown tool, each once.
    assert_eq!(x.refusals.len(), 2, "{:?}", x.refusals);
    assert!(
        x.refusals[0].starts_with("refused: create \"gold\" (coin) in #")
            && x.refusals[0].contains("not a container"),
        "{}",
        x.refusals[0]
    );
    assert!(
        x.refusals[1].starts_with("refused: no_such_tool: "),
        "{}",
        x.refusals[1]
    );
    assert_eq!(x.rules.len(), 1);
    assert!(
        x.rules[0].ends_with("minted: Kicking a rotten door"),
        "{}",
        x.rules[0]
    );
    assert_eq!(x.rolls.len(), 1);
    assert!(x.rolls[0].starts_with("roll: nerve d20 "), "{}", x.rolls[0]);
}

#[test]
fn the_footer_follows_the_spec_format() {
    let (game, seq) = played();
    let x = turn(game.world(), seq).unwrap().unwrap();
    let footer = footer(&x);
    let lines: Vec<&str> = footer.lines().collect();
    assert_eq!(lines[0], "[director · sonnet director 5 calls 0.0s");
    assert!(
        lines[1].starts_with(" in 6.0k (91% cached) out 250 · $0.0"),
        "{}",
        lines[1]
    );
    assert!(lines[1].ends_with(']'));
    assert!(lines[2].starts_with("[refused: create \"gold\""));
    assert!(lines[3].starts_with("[refused: no_such_tool"));
    assert!(lines[4].starts_with("[rule #") && lines[4].ends_with("Kicking a rotten door]"));
    assert!(lines[5].starts_with("[roll: nerve"));
    assert_eq!(lines.len(), 6);
}

#[test]
fn runs_group_consecutive_calls_of_a_role() {
    let call = |role: &str, model: &str, ms| CallLine {
        n: 1,
        id: 1,
        role: role.into(),
        model: model.into(),
        latency_ms: ms,
        prompt_tokens: 0,
        cached_tokens: 0,
        output_tokens: 0,
        tool_calls: 0,
        cost_usd: None,
    };
    let calls = [
        call("interpret", "haiku", 400),
        call("director", "sonnet", 1000),
        call("director", "sonnet", 2100),
        call("judge", "haiku", 300),
    ];
    assert_eq!(
        runs(&calls).join(" → "),
        "haiku interpret 0.4s → sonnet director 2 calls 3.1s → haiku judge 0.3s"
    );
}

#[test]
fn turns_before_recording_say_they_have_no_records() {
    let (mut game, client) = begun();
    game.set_recording(false);
    client.push(Response::canned_text("Water drips."));
    game.turn(&client, "listen", &mut |_| {}).unwrap();
    let seq = latest_turn(game.world()).unwrap().unwrap();
    game.world().set_recorded_from(seq + 1);
    let x = turn(game.world(), seq).unwrap().unwrap();
    assert!(!x.recorded);
    assert_eq!(footer(&x), format!("[director · {PRE_RECORDING}]"));
    assert!(context_list(&x).contains(PRE_RECORDING));
    let lines = panel_lines(&x);
    assert!(lines.iter().any(|(l, _)| l.contains(PRE_RECORDING)));
    // No cost line for a turn with no records (and never "$-0.000").
    assert_eq!(lines[1].0, "director");
    assert_eq!(x.cost_usd.to_string(), "0");
}

#[test]
fn the_seed_and_other_events_are_not_turns() {
    let (game, seq) = played();
    assert!(turn(game.world(), 1).unwrap().is_none());
    assert!(turn(game.world(), seq + 1).unwrap().is_none());
    assert_eq!(game.world().turn_seqs().unwrap(), [seq]);
    assert_eq!(step_turn(game.world(), seq, true).unwrap(), None);
}

#[test]
fn panel_lines_tag_each_calls_lines() {
    let (game, seq) = played();
    let x = turn(game.world(), seq).unwrap().unwrap();
    let lines = panel_lines(&x);
    let tagged: Vec<&String> = lines
        .iter()
        .filter(|(_, c)| *c == Some(1))
        .map(|(l, _)| l)
        .collect();
    assert_eq!(tagged.len(), 2);
    assert!(tagged[0].starts_with("2 director · sonnet · 0.0s · 1 tool call"));
    assert!(lines.iter().any(|(l, _)| l == "Refusals:"));
    assert!(
        lines
            .iter()
            .any(|(l, _)| l.starts_with("- refused: create"))
    );
}

#[test]
fn context_lists_calls_by_seq_and_number() {
    let (game, seq) = played();
    let x = turn(game.world(), seq).unwrap().unwrap();
    let text = context_list(&x);
    assert!(text.contains(&format!("#{seq}.1 director · sonnet · 0.0s · in 1.0k")));
    assert!(text.contains(&format!("#{seq}.5 director")));
    assert!(turn_call(game.world(), seq, 5).unwrap().is_some());
    assert!(turn_call(game.world(), seq, 6).unwrap().is_none());
    assert!(turn_call(game.world(), seq, 0).unwrap().is_none());
}

fn last_call_tree(game: &Game, seq: i64) -> Tree {
    let call = turn_call(game.world(), seq, 5).unwrap().unwrap();
    call_tree(game.world(), &call).unwrap().0
}

fn find(tree: &Tree, label: &str) -> usize {
    tree.nodes
        .iter()
        .position(|n| n.label.starts_with(label))
        .unwrap_or_else(|| panic!("no node {label}: {:#?}", tree.nodes))
}

#[test]
fn the_tree_lays_out_a_call_with_token_counts() {
    let (game, seq) = played();
    let tree = last_call_tree(&game, seq);
    let top: Vec<&str> = tree
        .nodes
        .iter()
        .filter(|n| n.depth == 0)
        .map(|n| n.label.as_str())
        .collect();
    assert_eq!(
        top,
        [
            "System prompt (same as the previous call's)",
            "Tools (tools)",
            "Turn message",
            "Earlier messages (8)",
            "Response (stop: end_turn)",
            "Usage",
        ]
        .map(|l| if l.starts_with("Tools") { top[1] } else { l })
    );
    assert!(top[1].starts_with("Tools ("));
    // The slice's sections, and the input last.
    for section in ["Time", "Player", "Location", "Here", "Player input"] {
        let i = find(&tree, section);
        assert_eq!(tree.nodes[i].depth, 1);
    }
    let earlier = find(&tree, "Earlier messages");
    let refused = find(&tree, "user: tool result apply_patch ✗ error");
    assert_eq!(tree.nodes[refused].parent, Some(earlier));
    // Prompt leaves share the recorded prompt tokens; the cache covers the
    // start (tools and system) before the messages.
    let prompt: u64 = ["System", "Tools", "Turn message", "Earlier"]
        .iter()
        .map(|l| tree.nodes[find(&tree, l)].tokens)
        .sum();
    assert!((1_390..=1_410).contains(&prompt), "{prompt}");
    // 1300 of 1400 prompt tokens were read from the cache: the tools come
    // first, so they are wholly cached; the cached parts add up.
    let tools = &tree.nodes[find(&tree, "Tools")];
    assert!(tools.tokens > 0 && tools.cached == tools.tokens);
    let cached: u64 = ["System", "Tools", "Turn message", "Earlier"]
        .iter()
        .map(|l| tree.nodes[find(&tree, l)].cached)
        .sum();
    assert!((1_290..=1_310).contains(&cached), "{cached}");
    let response = &tree.nodes[find(&tree, "Response")];
    assert_eq!(response.tokens, 50);
    assert_eq!(response.cached, 0);
}

#[test]
fn the_system_prompt_folds_when_it_matches_the_previous_calls() {
    let (game, _) = played();
    let calls = game.world().calls().unwrap();
    // The bible call is the first of its role.
    let bible = &calls[0];
    let (tree, banner) = call_tree(game.world(), bible).unwrap();
    assert!(banner.is_none());
    let system = &tree.nodes[find(&tree, "System prompt")];
    assert_eq!(system.label, "System prompt");
    assert!(!system.folded);
    // The opening's first director call has none before it; the next does.
    let mut directors = calls.iter().filter(|c| c.call.role == "director");
    let (tree, _) = call_tree(game.world(), directors.next().unwrap()).unwrap();
    assert!(!tree.nodes[find(&tree, "System prompt")].folded);
    let director = directors.next().unwrap();
    let (tree, _) = call_tree(game.world(), director).unwrap();
    assert!(tree.nodes[find(&tree, "System prompt")].folded);
}

#[test]
fn folding_hides_and_shows_rows() {
    let (game, seq) = played();
    let mut tree = last_call_tree(&game, seq);
    let before = tree.rows().len();
    let message = find(&tree, "Turn message");
    tree.toggle(message);
    let folded = tree.rows().len();
    assert!(folded < before);
    assert!(
        !tree
            .rows()
            .iter()
            .any(|r| tree.nodes[r.node].parent == Some(message))
    );
    tree.toggle(message);
    let input = find(&tree, "Player input");
    tree.toggle(input);
    assert_eq!(tree.rows().len(), before + tree.nodes[input].body.len());
    assert_eq!(tree.marker(input), "▾ ");
    let usage = find(&tree, "Usage");
    assert_eq!(tree.marker(usage), "▾ ");
}

#[test]
fn search_finds_and_reveals_folded_text() {
    let (game, seq) = played();
    let mut tree = last_call_tree(&game, seq);
    let hits = tree.search("1000 GOLD");
    let input = find(&tree, "Player input");
    assert!(hits.contains(&input), "{hits:?}");
    tree.toggle(find(&tree, "Turn message"));
    tree.reveal(input, "1000 gold");
    let rows = tree.rows();
    assert!(rows.iter().any(|r| r.node == input && r.line.is_some()));
    assert!(tree.search("").is_empty());
}

#[test]
fn text_and_raw_json_cover_the_whole_call() {
    let (game, seq) = played();
    let call = turn_call(game.world(), seq, 1).unwrap().unwrap();
    let (tree, _) = call_tree(game.world(), &call).unwrap();
    let text = tree.text();
    assert!(text.contains("│ You are the director of Loam"), "{text}");
    assert!(text.contains("Player input"));
    assert!(text.contains("cache read 900"));
    let raw: serde_json::Value = serde_json::from_str(&tree::raw_json(&call)).unwrap();
    assert_eq!(raw["role"], "director");
    assert!(
        raw["system"]
            .as_str()
            .unwrap()
            .starts_with("You are the director")
    );
    assert_eq!(raw["usage"]["cache_read_input_tokens"], 900);
}

#[test]
fn a_variant_with_a_changed_system_prompt_gets_a_banner() {
    let (game, seq) = played();
    let original = turn_call(game.world(), seq, 1).unwrap().unwrap();
    let mut variant = original.clone();
    assert_eq!(variant_banner(&variant, Some(&original)), None);
    variant.reroll_id = Some(1);
    assert_eq!(variant_banner(&variant, Some(&original)), None);
    variant.system_hash = Some("0000000000000000".into());
    assert!(
        variant_banner(&variant, Some(&original))
            .unwrap()
            .contains("differs from the original turn's")
    );
    // Through the store: a variant call keeps its banner-free state when
    // its prompt matches.
    game.world().mark_variant(original.id, 1, 1);
    let marked = game.world().call(original.id).unwrap().unwrap();
    assert_eq!(marked.reroll_id, Some(1));
}

#[test]
fn sections_split_at_headings_only() {
    let text = "Glossary (lookup_terms; ? = undefined): wet\nTime: Day 1 · morning\nPlayer: #2 you\nStats (check, adjust_stat):\n  nerve 3\n\
                Recent:\n> look\nYou see nothing much. Then: a heron.\nPlayer input: hi";
    let s = sections(text);
    let labels: Vec<&str> = s.iter().map(|(l, _)| l.as_str()).collect();
    assert_eq!(
        labels,
        [
            "Glossary",
            "Time",
            "Player",
            "Stats",
            "Recent",
            "Player input"
        ]
    );
    assert_eq!(s[4].1.len(), 3);
}

#[test]
fn token_counts_read_short() {
    assert_eq!(tokens(410), "410");
    assert_eq!(tokens(6_200), "6.2k");
    assert_eq!(tokens(128_400), "128k");
    assert_eq!(short_model("claude-haiku-5-5"), "haiku");
    assert_eq!(short_model("fake"), "fake");
}
