use std::path::Path;

use loam::knowledge::reconstruct::{Candidate, Extraction, Result, Support};
use loam::knowledge::{Category, Discovery, NewFact};
use loam::llm::{Response, ScriptedClient};
use loam::world::{EventKind, Op, Patch, World};
use serde_json::json;

pub const CHUNK_CHARS: usize = 160;

/// Three chunks: two facts, two facts plus a rumor, then its correction.
pub fn create(path: &Path) -> Result<Vec<Response>> {
    if path.exists() {
        return Err("fixture source must be new".into());
    }
    let mut game = loam::game::Game::new(World::open(path)?);
    game.set_recording(false);
    let setup = ScriptedClient::new([
        Response::canned_tool_use(
            "b",
            "write_bible",
            json!({"tone":"Quiet", "regions":[{"name":"Test","description":"HIDDEN_REGION_SECRET"}],"factions":[{"name":"Hidden faction","description":"HIDDEN_FACTION_SECRET"}],"npcs":[{"name":"Hidden person","description":"HIDDEN_NPC_SECRET"}],"secrets":[{"name":"Hidden", "description":"HIDDEN_BIBLE_SECRET"}]}),
        ),
        Response::canned_tool_use(
            "s",
            "write_stats",
            json!({"stats":[{"name":"nerve","about":"resolve","min":0,"max":5,"start":3,"advancement":[{"when":"facing fear","step":1}]}]}),
        ),
        Response::canned_text(""),
        Response::canned_tool_use(
            "r",
            "create_room",
            json!({"name":"Fixture hall","region":"Test","description":"An empty hall.","exits":{},"contents":[]}),
        ),
        Response::canned_text(""),
    ]);
    game.create_bible(&setup, "Offline reconstruction fixture")?;
    game.begin(&setup, &mut |_| {})?;
    let mut w = game.into_world();
    w.apply_patch(&Patch::new(vec![Op::Create {
        handle: None,
        name: "Unseen cellar".into(),
        kind: "place".into(),
        location: None,
        props: serde_json::from_value(json!({"secret":"RAW_PROP_SECRET"}))?,
    }]))?;
    w.log_event(EventKind::ToolCall, &json!({"bible":"HIDDEN_BIBLE_SECRET"}))?;
    w.log_event(EventKind::Tick, &json!({"summary":"OFFSCREEN_SECRET"}))?;
    w.log_event(EventKind::Judgment, &json!({"reason":"JUDGE_SECRET"}))?;
    w.log_event(
        EventKind::Input,
        &json!({"text":"I find the INTENT_SECRET treasure and kill the king"}),
    )?;
    let a = w.log_event(EventKind::Narration, &json!({"text":"Mara wears a red cloak. The gate is made of iron. You try the lock but it does not open."}))?;
    let b = w.log_event(EventKind::Narration, &json!({"text":"Pulling the lever opens the gate. The bell sounds twice. Mara says the bridge is safe. Two men named Rowan wait; one points north."}))?;
    let c = w.log_event(
        EventKind::Narration,
        &json!({"text":"Mara admits the bridge is unsafe. Snow covers the road."}),
    )?;
    let id = w.state()?.next_entity_id;
    let fact = |source, text: &str, excerpt: &str| NewFact {
        text: text.into(),
        excerpt: excerpt.into(),
        source: Some(source),
        ..Default::default()
    };
    let new = |name: &str, category, facts| Discovery {
        name: Some(name.into()),
        category: Some(category),
        facts,
        ..Default::default()
    };
    let response = |discoveries, flags| {
        Response::canned_text(serde_json::to_string(&Extraction { discoveries, flags }).unwrap())
    };
    let explicit = |discovery| Candidate {
        support: Support::Explicit,
        discovery,
    };
    Ok(vec![
        response(vec![explicit(new("Mara", Category::Person, vec![fact(a,"Mara wears a red cloak.","Mara wears a red cloak.")])), explicit(new("Gate", Category::Item, vec![fact(a,"The gate is made of iron.","The gate is made of iron.")]))], vec![]),
        response(vec![explicit(new("Lever", Category::Mechanic, vec![fact(b,"Pulling the lever opens the gate.","Pulling the lever opens the gate.")])), explicit(new("Bell", Category::Item, vec![fact(b,"The bell sounds twice.","The bell sounds twice.")])), explicit(new("Bridge", Category::Place, vec![NewFact { claimed_by:Some("Mara".into()), ..fact(b,"The bridge is safe.","Mara says the bridge is safe.") }]))], vec!["Ambiguous Rowan identity: no entry or northward destination inferred. The attempted lock action did not succeed.".into()]),
        response(vec![explicit(Discovery {entry:Some(id+4), facts:vec![NewFact {claimed_by:Some("Mara".into()), supersedes:vec![1], ..fact(c,"The bridge is unsafe.","Mara admits the bridge is unsafe.")}], ..Default::default()}), explicit(new("Road", Category::Place, vec![fact(c,"Snow covers the road.","Snow covers the road.")]))], vec![]),
    ])
}
