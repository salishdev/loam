//! One-off migration of `drowned-mill.db` (made before schema versions) to
//! the current store layout. Not part of the engine; delete it once no
//! save needs it.
//!
//! ```text
//! cargo run --example migrate_drowned_mill -- OLD.db NEW.db
//! ```
//!
//! The old store is only read. The new store's log keeps the old events
//! that change no state (inputs, narration, ticks, rolls, rule changes,
//! tool calls) with their original numbers and times, then one engine patch
//! that recreates the migrated state with the same entity ids. The new
//! store is built by replaying that log, so the validator checks the patch.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::path::Path;

use loam::glossary::{self, Term};
use loam::quest::CONCERNS_REL;
use loam::world::{EXIT_REL, EntityId, Op, Patch, World};
use rusqlite::{Connection, OpenFlags, params};
use serde_json::{Map, Value, json};

type Res<T> = Result<T, Box<dyn Error>>;

/// Prop keys that become part of the `_quest` component.
const QUEST_PROPS: [&str; 5] = ["_status", "_hook", "_goal", "_developments", "_resolution"];

struct Old {
    id: EntityId,
    name: String,
    kind: String,
    location: Option<EntityId>,
    props: Map<String, Value>,
}

fn main() -> Res<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [old_path, new_path] = args.as_slice() else {
        return Err("usage: migrate_drowned_mill OLD.db NEW.db".into());
    };
    if Path::new(new_path).exists() {
        return Err(format!("{new_path} already exists").into());
    }
    let old = Connection::open_with_flags(old_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let has_version: bool = old.query_row(
        "SELECT EXISTS (SELECT 1 FROM meta WHERE key = 'schema_version')",
        [],
        |r| r.get(0),
    )?;
    if has_version {
        return Err("this store already has a schema version; it isn't the old layout".into());
    }

    let entities = read_entities(&old)?;
    let patch = migrate(&entities)?;

    // A log-only staging store, then a replay of it into the new store.
    let staging = format!("{new_path}.staging");
    let _ = std::fs::remove_file(&staging);
    drop(World::open(&staging)?); // creates the tables and stamps the version
    let last = write_log(&old, &staging, &patch)?;
    let migrated = World::open(&staging)?.fork(last, new_path)?;
    drop(migrated);
    std::fs::remove_file(&staging)?;
    // It's the same game, not a branch of one.
    Connection::open(new_path)?.execute("DELETE FROM meta WHERE key = 'forked_at'", [])?;

    let world = World::open(new_path)?;
    if world.replay_in_memory()?.state()? != world.state()? {
        return Err("migrated store doesn't replay to itself".into());
    }
    let state = world.state()?;
    println!(
        "migrated {old_path} -> {new_path}: {} entities, {} relations, {} events",
        state.entities.len(),
        state.relations.len(),
        world.last_seq()?
    );
    Ok(())
}

fn read_entities(conn: &Connection) -> Res<Vec<Old>> {
    let mut out: Vec<Old> = conn
        .prepare("SELECT id, name, kind, location FROM entities ORDER BY id")?
        .query_map([], |r| {
            Ok(Old {
                id: r.get(0)?,
                name: r.get(1)?,
                kind: r.get(2)?,
                location: r.get(3)?,
                props: Map::new(),
            })
        })?
        .collect::<Result<_, _>>()?;
    for (i, e) in out.iter_mut().enumerate() {
        // Recreating entities in order must give each its old id.
        if e.id != i as EntityId + 1 {
            return Err(format!("entity ids aren't contiguous at #{}", e.id).into());
        }
        let rows: Vec<(String, String)> = conn
            .prepare("SELECT key, value FROM props WHERE entity_id = ?1 ORDER BY key")?
            .query_map([e.id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        for (k, v) in rows {
            e.props.insert(k, serde_json::from_str(&v)?);
        }
    }
    Ok(out)
}

/// One engine patch recreating the migrated world.
fn migrate(entities: &[Old]) -> Res<Patch> {
    let mut creates = Vec::new();
    let mut moves = Vec::new();
    let mut relations = Vec::new();
    let mut stubs = Vec::new();
    let mut next_id = entities.len() as EntityId + 1;

    let holders: BTreeSet<EntityId> = entities.iter().filter_map(|e| e.location).collect();

    for e in entities {
        let mut props = e.props.clone();
        match e.kind.as_str() {
            "npc" => {
                props.insert("acts".into(), json!(true));
            }
            "quest" => {
                let mut state = Map::new();
                for key in QUEST_PROPS {
                    if let Some(v) = props.remove(key) {
                        state.insert(key[1..].into(), v);
                    }
                }
                props.insert("_quest".into(), Value::Object(state));
                let concerns: Vec<EntityId> = match props.remove("_concerns") {
                    Some(v) => serde_json::from_value(v)?,
                    None => Vec::new(),
                };
                for c in concerns {
                    relations.push(relate(e.id, CONCERNS_REL, c, Map::new()));
                }
            }
            "rule" => {
                let class = props
                    .get("_rule")
                    .and_then(|r| r.get("class"))
                    .and_then(Value::as_str);
                if class != Some("stat") {
                    // World rules may pin entities by id; this save has none.
                    return Err(
                        format!("rule #{} isn't a stat rule; pins aren't migrated", e.id).into(),
                    );
                }
            }
            "glossary" => props = Map::from_iter([("_terms".into(), terms(&props)?)]),
            _ => {}
        }
        match props.remove("portable") {
            Some(Value::Bool(false)) => {
                props.insert("fixed".into(), json!(true));
            }
            Some(Value::Bool(true)) | None => {}
            Some(v) => return Err(format!("#{}: portable = {v}", e.id).into()),
        }
        if let Some(exits) = props.remove("exits") {
            let Value::Object(exits) = exits else {
                return Err(format!("#{}: exits isn't an object", e.id).into());
            };
            for (dir, exit) in exits {
                let label = exit["label"].as_str().unwrap_or_default().to_owned();
                let to = match exit.get("to").and_then(Value::as_i64) {
                    Some(to) => to,
                    None => {
                        let id = next_id;
                        next_id += 1;
                        stubs.push(Op::Create {
                            handle: None,
                            name: label.clone(),
                            kind: "room".into(),
                            location: None,
                            props: Map::from_iter([("_unexplored".into(), json!(true))]),
                        });
                        id
                    }
                };
                let Value::Object(p) = json!({"dir": dir, "label": label}) else {
                    unreachable!()
                };
                relations.push(relate(e.id, EXIT_REL, to, p));
            }
        }
        let implicit = matches!(e.kind.as_str(), "room" | "player")
            || props.get("_carrier") == Some(&json!(true))
            || props.get("acts") == Some(&json!(true));
        if holders.contains(&e.id) && !implicit {
            props.insert("container".into(), json!(true));
        }
        creates.push(Op::Create {
            handle: None,
            name: e.name.clone(),
            kind: e.kind.clone(),
            location: None,
            props,
        });
        if let Some(to) = e.location {
            moves.push(Op::Move {
                entity: e.id.into(),
                to: Some(to.into()),
            });
        }
    }

    let mut ops = creates;
    ops.extend(stubs);
    ops.extend(moves);
    ops.extend(relations);
    Ok(Patch::new(ops))
}

fn relate(subject: EntityId, rel: &str, object: EntityId, props: Map<String, Value>) -> Op {
    Op::Relate {
        subject: subject.into(),
        rel: rel.into(),
        object: object.into(),
        props,
    }
}

/// The old one-prop-per-term glossary as a `_terms` component: the
/// built-in terms (markers, `exit`), plus every old term except the
/// retired `exits` and `portable`.
fn terms(old: &Map<String, Value>) -> Res<Value> {
    let mut all: BTreeMap<String, Term> = glossary::builtin_terms();
    for (name, v) in old {
        if name == "exits" || name == "portable" || all.contains_key(name) {
            continue;
        }
        all.insert(name.clone(), serde_json::from_value(v.clone())?);
    }
    Ok(serde_json::to_value(all)?)
}

/// Copy the old events that change no state into the staging store, then
/// append the migration patch. Returns the last event's number.
fn write_log(old: &Connection, staging: &str, patch: &Patch) -> Res<i64> {
    let mut conn = Connection::open(staging)?;
    let tx = conn.transaction()?;
    {
        let mut stmt = old.prepare(
            "SELECT seq, kind, payload, at FROM events
             WHERE kind NOT IN ('patch', 'patch_rejected') ORDER BY seq",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let (seq, kind, payload, at): (i64, String, String, String) =
                (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?);
            tx.execute(
                "INSERT INTO events (seq, kind, payload, at) VALUES (?1, ?2, ?3, ?4)",
                params![seq, kind, payload, at],
            )?;
        }
    }
    let old_last: i64 =
        old.query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |r| r.get(0))?;
    let created: Vec<EntityId> = patch
        .ops
        .iter()
        .filter(|op| matches!(op, Op::Create { .. }))
        .enumerate()
        .map(|(i, _)| i as EntityId + 1)
        .collect();
    let payload = json!({
        "patch": patch,
        "created": created,
        "handles": {},
        "by": "engine",
        "migration": "drowned-mill.db from before schema versions; \
                      old patches dropped, other events kept",
    });
    let last = old_last + 1;
    tx.execute(
        "INSERT INTO events (seq, kind, payload) VALUES (?1, 'patch', ?2)",
        params![last, payload.to_string()],
    )?;
    tx.commit()?;
    Ok(last)
}
