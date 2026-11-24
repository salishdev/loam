#[path = "../examples/support/reconstruction_fixture.rs"]
mod fixture;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use loam::knowledge::reconstruct::{Options, reconstruct};
use loam::knowledge::{self, Category};
use loam::llm::{ContentBlock, Message, Response, ScriptedClient, StopReason};
use loam::world::World;
use serde_json::{Value, json};

struct Case {
    dir: PathBuf,
    source: PathBuf,
    output: PathBuf,
    report: PathBuf,
    responses: Vec<Response>,
}
impl Case {
    fn new() -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "loam-reconstruct-{}-{}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&dir).unwrap();
        let source = dir.join("source.db");
        let responses = fixture::create(&source).unwrap();
        Self {
            output: dir.join("output.db"),
            report: dir.join("evidence.txt"),
            dir,
            source,
            responses,
        }
    }
    fn run(&self, client: &ScriptedClient) -> loam::knowledge::reconstruct::Result<()> {
        reconstruct(
            &self.source,
            &self.output,
            &self.report,
            client,
            Options {
                chunk_chars: fixture::CHUNK_CHARS,
            },
        )
    }
    fn text(&self) -> String {
        std::fs::read_to_string(&self.report).unwrap()
    }
}
impl Drop for Case {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
fn message_text(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}
fn bytes(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap()
}
fn mutate(response: &Response, change: impl FnOnce(&mut Value)) -> Response {
    let mut value: Value = serde_json::from_str(&response.text()).unwrap();
    change(&mut value);
    Response::canned_text(value.to_string())
}
fn tool_response(response: &Response) -> Response {
    Response::canned_tool_use(
        "extraction",
        "record_discoveries",
        serde_json::from_str(&response.text()).unwrap(),
    )
}

#[test]
fn typed_result_tool_imports_the_fixture_and_declares_nested_evidence_schema() {
    let c = Case::new();
    let original = bytes(&c.source);
    let client = ScriptedClient::new(c.responses.iter().map(|response| {
        let mut result = tool_response(response);
        result.content.push(ContentBlock::text(
            "Accompanying prose is not the result JSON.",
        ));
        result
    }));
    c.run(&client).unwrap();
    assert_eq!(bytes(&c.source), original);
    let world = World::open(&c.output).unwrap();
    assert_eq!(knowledge::entries(&world).unwrap().len(), 6);
    assert_eq!(
        world.state().unwrap(),
        world.replay_in_memory().unwrap().state().unwrap()
    );
    assert!(
        knowledge::compendium(&world, "player Bridge")
            .unwrap()
            .contains("corrects 1")
    );
    assert!(c.text().starts_with("Status: COMPLETE"));
    for request in client.requests() {
        assert_eq!(request.tools.len(), 1);
        let tool = &request.tools[0];
        assert_eq!(tool.name, "record_discoveries");
        let schema = &tool.input_schema;
        assert_eq!(schema["required"], json!(["discoveries", "flags"]));
        assert_eq!(schema["additionalProperties"], false);
        let candidate = &schema["properties"]["discoveries"]["items"];
        assert_eq!(candidate["required"], json!(["support", "discovery"]));
        assert_eq!(
            candidate["properties"]["support"]["enum"],
            json!(["explicit", "ambiguous", "inferred"])
        );
        let discovery = &candidate["properties"]["discovery"];
        assert_eq!(discovery["required"], json!(["facts"]));
        let props = &discovery["properties"];
        assert_eq!(props["entry"]["type"], "integer");
        for forbidden in ["subject", "rule", "term"] {
            assert!(props.get(forbidden).is_none());
        }
        let fact = &props["facts"]["items"];
        assert_eq!(fact["required"], json!(["text", "excerpt", "source"]));
        assert_eq!(fact["properties"]["source"]["type"], "integer");
        assert_eq!(fact["additionalProperties"], false);
        let instructions = request.system.unwrap();
        assert!(instructions.contains("ONE discovery"));
        assert!(instructions.contains("numeric existing IDs listed in prior_entries"));
    }
}

fn malformed(response: &Response, shape: &str) -> Response {
    mutate(response, |v| match shape {
        "entry_name" => v["discoveries"][1]["discovery"]["entry"] = json!("Dry oak peg"),
        "flat" => v["discoveries"][1] = v["discoveries"][1]["discovery"].clone(),
        "support" => {
            v["discoveries"][1]
                .as_object_mut()
                .unwrap()
                .remove("support");
        }
        "discovery" => {
            v["discoveries"][1]
                .as_object_mut()
                .unwrap()
                .remove("discovery");
        }
        "flags" => {
            v.as_object_mut().unwrap().remove("flags");
        }
        "unknown" => v["discoveries"][1]["discovery"]["facts"][0]["extra"] = json!(true),
        _ => unreachable!(),
    })
}

#[test]
fn malformed_response_gets_one_fresh_repair_without_importing_any_of_its_candidates() {
    for shape in [
        "entry_name",
        "flat",
        "support",
        "discovery",
        "flags",
        "unknown",
    ] {
        for tool in [false, true] {
            let c = Case::new();
            let original = bytes(&c.source);
            let bad = malformed(&c.responses[0], shape);
            let bad = if tool { tool_response(&bad) } else { bad };
            let client = ScriptedClient::new(
                [bad]
                    .into_iter()
                    .chain(c.responses.iter().map(tool_response)),
            );
            c.run(&client).unwrap();
            assert_eq!(bytes(&c.source), original);
            let requests = client.requests();
            assert_eq!(requests.len(), 4, "{shape}");
            assert_eq!(requests[0].tools, requests[1].tools);
            assert_eq!(requests[0].messages[0], requests[1].messages[0]);
            assert_eq!(requests[1].messages.len(), 2);
            assert!(message_text(&requests[1].messages[1]).contains("structure was invalid"));
            let world = World::open(&c.output).unwrap();
            assert_eq!(knowledge::entries(&world).unwrap().len(), 6);
            assert_eq!(c.text().matches("Imported entry #").count(), 7);
        }
    }
}

#[test]
fn repeated_malformed_results_stop_after_two_calls_with_no_discoveries_written() {
    for shape in [
        "entry_name",
        "flat",
        "support",
        "discovery",
        "flags",
        "unknown",
    ] {
        let c = Case::new();
        let original = bytes(&c.source);
        let bad = tool_response(&malformed(&c.responses[0], shape));
        let client = ScriptedClient::new([bad.clone(), bad, tool_response(&c.responses[0])]);
        assert!(c.run(&client).is_err(), "{shape}");
        assert_eq!(client.requests().len(), 2);
        assert_eq!(client.remaining(), 1);
        assert_eq!(bytes(&c.source), original);
        assert!(
            knowledge::entries(&World::open(&c.output).unwrap())
                .unwrap()
                .is_empty()
        );
        assert!(c.text().contains("FAILED / INCOMPLETE"));
        assert!(!c.text().contains("Imported entry #"));
        assert!(!c.text().contains("COMPLETE: replay matches"));
    }
}

#[test]
fn only_one_declared_result_tool_with_a_matching_stop_reason_is_accepted() {
    for failure in [
        "unknown",
        "multiple",
        "mixed",
        "end_turn",
        "truncated",
        "missing_stop",
        "no_call",
    ] {
        let c = Case::new();
        let original = bytes(&c.source);
        let mut response = tool_response(&c.responses[0]);
        match failure {
            "unknown" => {
                if let ContentBlock::ToolUse(call) = &mut response.content[0] {
                    call.name = "learn".into();
                }
            }
            "multiple" => response.content.push(response.content[0].clone()),
            "mixed" => response
                .content
                .push(ContentBlock::ToolUse(loam::llm::ToolUse {
                    id: "other".into(),
                    name: "learn".into(),
                    input: json!({}),
                })),
            "end_turn" => response.stop_reason = Some(StopReason::EndTurn),
            "truncated" => response.stop_reason = Some(StopReason::MaxTokens),
            "missing_stop" => response.stop_reason = None,
            "no_call" => {
                response.content.clear();
            }
            _ => unreachable!(),
        }
        // Even valid accompanying text cannot rescue an invalid tool protocol.
        response
            .content
            .push(ContentBlock::text(c.responses[0].text()));
        let client = ScriptedClient::new([response, tool_response(&c.responses[0])]);
        assert!(c.run(&client).is_err(), "{failure}");
        assert_eq!(client.requests().len(), 1, "{failure}");
        assert_eq!(bytes(&c.source), original);
        assert!(
            knowledge::entries(&World::open(&c.output).unwrap())
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn chronological_extraction_uses_live_records_reports_and_replay_without_touching_source() {
    let c = Case::new();
    let original = bytes(&c.source);
    let client = ScriptedClient::new(c.responses.clone());
    c.run(&client).unwrap();
    assert_eq!(bytes(&c.source), original);
    assert!(!c.source.with_extension("db-wal").exists());
    let w = World::open(&c.output).unwrap();
    let entries = knowledge::entries(&w).unwrap();
    assert_eq!(entries.len(), 6);
    assert!(entries.iter().any(|e| e.category == Category::Mechanic));
    for e in &entries {
        for f in &e.facts {
            for s in &f.sources {
                let event = w.event(s.event).unwrap().unwrap();
                assert!(
                    event.payload["text"]
                        .as_str()
                        .unwrap()
                        .contains(s.excerpt.as_ref().unwrap())
                );
            }
        }
        assert!(!e.linked);
    }
    let bridge = knowledge::compendium(&w, "player Bridge").unwrap();
    assert!(bridge.contains("Mara claims:"));
    assert!(bridge.contains("superseded by 2"));
    assert!(bridge.contains("corrects 1"));
    assert!(bridge.contains("Mara admits the bridge is unsafe."));
    for name in ["Mara", "Gate", "Lever", "Bell", "Road"] {
        let report = knowledge::compendium(&w, &format!("player {name}")).unwrap();
        assert!(report.contains("[#"));
        assert!(report.contains('"'), "narration excerpt visible: {report}");
    }
    assert!(
        !knowledge::compendium(&w, "player")
            .unwrap()
            .contains("Unseen cellar")
    );
    assert!(
        knowledge::compendium(&w, "world")
            .unwrap()
            .contains("Unseen cellar")
    );
    assert_eq!(
        w.state().unwrap(),
        w.replay_in_memory().unwrap().state().unwrap()
    );
    let fork = w.fork_in_memory(w.last_seq().unwrap()).unwrap();
    assert_eq!(knowledge::entries(&fork).unwrap(), entries);
    assert_eq!(w.state().unwrap(), fork.state().unwrap());
    assert!(c.text().contains("COMPLETE: replay matches"));
    assert!(c.text().contains("Ambiguous Rowan"));
    assert_eq!(client.requests().len(), 3);
    for request in client.requests() {
        let text = format!("{:?}", request.messages);
        for secret in [
            "RAW_PROP_SECRET",
            "HIDDEN_BIBLE_SECRET",
            "OFFSCREEN_SECRET",
            "JUDGE_SECRET",
            "INTENT_SECRET",
            "Unseen cellar",
        ] {
            assert!(!text.contains(secret), "leaked {secret}");
        }
        let v: Value = serde_json::from_str(&message_text(&request.messages[0])).unwrap();
        let size: usize = v["narration"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["text"].as_str().unwrap().chars().count())
            .sum();
        assert!(size <= fixture::CHUNK_CHARS);
    }
    let last = &client.requests()[2];
    assert!(message_text(&last.messages[0]).contains("The bridge is safe."));
    assert!(message_text(&last.messages[0]).contains("supersedes"));
}

#[test]
fn evidence_and_identity_errors_fail_with_incomplete_artifacts_and_unchanged_source() {
    for bad in [
        "missing",
        "wrong_excerpt",
        "short_excerpt",
        "empty_excerpt",
        "wrong_event",
        "future",
        "subject",
        "rule",
        "unknown_entry",
        "missing_source",
    ] {
        let c = Case::new();
        let original = bytes(&c.source);
        let response = mutate(&c.responses[0], |v| {
            let d = &mut v["discoveries"][0]["discovery"];
            match bad {
                "missing" => d["facts"][0]["source"] = json!(9999),
                "wrong_excerpt" => d["facts"][0]["excerpt"] = json!("She has a blue cloak"),
                "short_excerpt" => d["facts"][0]["excerpt"] = json!("Mara wears"),
                "empty_excerpt" => d["facts"][0]["excerpt"] = json!(""),
                "wrong_event" => d["facts"][0]["source"] = json!(5), // player input, not narration
                "future" => d["facts"][0]["source"] = json!(8),
                "subject" => d["subject"] = json!(1),
                "rule" => d["rule"] = json!(1),
                "unknown_entry" => {
                    d["entry"] = json!(9999);
                    d.as_object_mut().unwrap().remove("name");
                    d.as_object_mut().unwrap().remove("category");
                }
                "missing_source" => {
                    d["facts"][0].as_object_mut().unwrap().remove("source");
                }
                _ => unreachable!(),
            }
        });
        let client = ScriptedClient::new([tool_response(&response)]);
        assert!(c.run(&client).is_err(), "{bad}");
        assert_eq!(
            client.requests().len(),
            1,
            "evidence failures are not retried"
        );
        assert_eq!(bytes(&c.source), original);
        assert!(c.output.exists());
        assert!(c.text().contains("FAILED / INCOMPLETE"));
        assert!(!c.text().contains("COMPLETE: replay matches"));
        assert!(
            knowledge::entries(&World::open(&c.output).unwrap())
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn inferred_and_ambiguous_candidates_are_reported_without_becoming_knowledge() {
    let c = Case::new();
    let first = mutate(&c.responses[0], |v| {
        v["discoveries"][0]["support"] = json!("inferred");
        v["discoveries"][0]["discovery"]["facts"][0]["text"] =
            json!("The king was killed and treasure found.");
        v["discoveries"][1]["support"] = json!("ambiguous");
    });
    let empty = Response::canned_text(r#"{"discoveries":[],"flags":[]}"#);
    c.run(&ScriptedClient::new([
        tool_response(&first),
        empty.clone(),
        empty,
    ]))
    .unwrap();
    assert!(
        knowledge::entries(&World::open(&c.output).unwrap())
            .unwrap()
            .is_empty()
    );
    assert!(c.text().contains("Inferred, not imported"));
    assert!(c.text().contains("Ambiguous, not imported"));
}

#[test]
fn partial_extraction_is_not_success_and_cannot_be_reimported_even_after_fork() {
    let c = Case::new();
    let original = bytes(&c.source);
    let client = ScriptedClient::new([c.responses[0].clone()]);
    assert!(c.run(&client).is_err());
    assert_eq!(bytes(&c.source), original);
    assert!(c.text().contains("FAILED / INCOMPLETE"));
    let partial = World::open(&c.output).unwrap();
    assert_eq!(knowledge::entries(&partial).unwrap().len(), 2);
    let fork_path = c.dir.join("fork.db");
    drop(
        partial
            .fork(partial.last_seq().unwrap(), &fork_path)
            .unwrap(),
    );
    drop(partial);
    for (i, input) in [&c.output, &fork_path].into_iter().enumerate() {
        let result = reconstruct(
            input,
            &c.dir.join(format!("repeat-{i}.db")),
            &c.dir.join(format!("repeat-{i}.txt")),
            &ScriptedClient::default(),
            Options::default(),
        );
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("already reconstructed")
        );
    }
}

#[test]
fn refuses_existing_paths_unsupported_versions_and_active_sidecars_before_extraction() {
    for target in ["output", "report", "version", "sidecar"] {
        let c = Case::new();
        match target {
            "output" => std::fs::write(&c.output, b"do not overwrite").unwrap(),
            "report" => std::fs::write(&c.report, b"do not overwrite").unwrap(),
            "version" => {
                let db = rusqlite::Connection::open(&c.source).unwrap();
                db.execute("UPDATE meta SET value='5' WHERE key='schema_version'", [])
                    .unwrap();
            }
            "sidecar" => std::fs::write(c.source.with_extension("db-wal"), b"active").unwrap(),
            _ => unreachable!(),
        }
        let original = bytes(&c.source);
        let client = ScriptedClient::default();
        let error = c.run(&client).unwrap_err().to_string();
        assert_eq!(bytes(&c.source), original);
        assert!(client.requests().is_empty());
        match target {
            "output" => assert_eq!(bytes(&c.output), b"do not overwrite"),
            "report" => assert_eq!(bytes(&c.report), b"do not overwrite"),
            "version" => assert!(error.contains("unsupported input schema 5")),
            "sidecar" => assert!(error.contains("close the source")),
            _ => unreachable!(),
        }
    }
}

#[test]
fn repeat_success_refuses_and_truncated_model_response_never_imports() {
    let c = Case::new();
    c.run(&ScriptedClient::new(c.responses.clone())).unwrap();
    let original = bytes(&c.output);
    let result = reconstruct(
        &c.output,
        &c.dir.join("repeat.db"),
        &c.dir.join("repeat.txt"),
        &ScriptedClient::default(),
        Options::default(),
    );
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("already reconstructed")
    );
    assert_eq!(bytes(&c.output), original);
    let c = Case::new();
    let mut response = c.responses[0].clone();
    response.stop_reason = Some(StopReason::MaxTokens);
    assert!(
        c.run(&ScriptedClient::new([response]))
            .unwrap_err()
            .to_string()
            .contains("did not finish")
    );
    assert!(
        knowledge::entries(&World::open(&c.output).unwrap())
            .unwrap()
            .is_empty()
    );
}
