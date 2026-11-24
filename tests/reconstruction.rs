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
        let client = ScriptedClient::new([response]);
        assert!(c.run(&client).is_err(), "{bad}");
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
    c.run(&ScriptedClient::new([first, empty.clone(), empty]))
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
