//! Model-call records (`llm_calls`) and the prompts they share
//! (`llm_prompts`).
//!
//! Records are not events: replay ignores them. They hang off the log by
//! `turn_seq` (the turn's `input` event) and `log_seq` (the last event when
//! the record was written), which is what a fork copies by.

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::Value;

use super::{StoreError, World};
use crate::llm::{CallRecord, Usage};

/// A recorded call as read back from the store, its system prompt and tool
/// definitions filled in from `llm_prompts`.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredCall {
    pub id: i64,
    /// The turn's `input` event; `None` for the bible and the opening.
    pub turn_seq: Option<i64>,
    pub reroll_id: Option<i64>,
    pub variant: Option<i64>,
    /// The last event in the log when the call was recorded.
    pub log_seq: i64,
    pub system_hash: Option<String>,
    pub tools_hash: Option<String>,
    pub created_at: String,
    pub call: CallRecord,
}

const COLUMNS: &str = "id, turn_seq, reroll_id, variant, log_seq, role, model, effort, \
    system_hash, tools_hash, request, response, input_tokens, output_tokens, \
    cache_creation_tokens, cache_read_tokens, latency_ms, cost_usd, created_at";

impl World {
    /// Store calls made for the turn whose input is event `turn_seq`
    /// (`None` for the bible and the opening), in order. System prompts and
    /// tool definitions are stored once each, by hash.
    pub fn record_calls(
        &self,
        turn_seq: Option<i64>,
        calls: &[CallRecord],
    ) -> Result<(), StoreError> {
        if calls.is_empty() {
            return Ok(());
        }
        let tx = self.conn.unchecked_transaction()?;
        let log_seq = self.last_seq()?;
        for call in calls {
            let system_hash = call
                .system
                .as_deref()
                .map(|s| store_prompt(&tx, s))
                .transpose()?;
            let tools_hash = call
                .tools
                .as_ref()
                .map(|t| store_prompt(&tx, &t.to_string()))
                .transpose()?;
            tx.execute(
                "INSERT INTO llm_calls (turn_seq, log_seq, role, model, effort, system_hash,
                    tools_hash, request, response, input_tokens, output_tokens,
                    cache_creation_tokens, cache_read_tokens, latency_ms, cost_usd)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                params![
                    turn_seq,
                    log_seq,
                    call.role,
                    call.model,
                    call.effort,
                    system_hash,
                    tools_hash,
                    call.request.to_string(),
                    call.response.to_string(),
                    call.usage.input_tokens as i64,
                    call.usage.output_tokens as i64,
                    call.usage.cache_creation_input_tokens as i64,
                    call.usage.cache_read_input_tokens as i64,
                    call.latency_ms as i64,
                    call.cost_usd,
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Every recorded call, in order.
    pub fn calls(&self) -> Result<Vec<StoredCall>, StoreError> {
        self.query_calls("1 = 1", [])
    }

    /// The calls recorded for the turn whose input is event `turn_seq`, in
    /// order (rerolls' variant calls left out).
    pub fn turn_calls(&self, turn_seq: i64) -> Result<Vec<StoredCall>, StoreError> {
        self.query_calls("turn_seq = ?1 AND reroll_id IS NULL", [turn_seq])
    }

    /// One recorded call by id.
    pub fn call(&self, id: i64) -> Result<Option<StoredCall>, StoreError> {
        Ok(self.query_calls("id = ?1", [id])?.into_iter().next())
    }

    /// The latest call before call `id` made for the same role, if any.
    pub fn previous_call(&self, id: i64, role: &str) -> Result<Option<StoredCall>, StoreError> {
        Ok(self
            .query_calls(
                "id = (SELECT MAX(id) FROM llm_calls WHERE id < ?1 AND role = ?2)",
                params![id, role],
            )?
            .into_iter()
            .next())
    }

    /// The `input` events of played turns (the seed left out), in order.
    pub fn turn_seqs(&self) -> Result<Vec<i64>, StoreError> {
        Ok(self
            .conn
            .prepare(
                "SELECT seq FROM events WHERE kind = 'input'
                   AND COALESCE(json_extract(payload, '$.seed'), 0) != 1 ORDER BY seq",
            )?
            .query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?)
    }

    /// Say that recording began at event `seq`, as an upgrade from schema 5
    /// does; tests use it to make older turns.
    #[cfg(test)]
    pub(crate) fn set_recorded_from(&self, seq: i64) {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES ('recorded_from', ?1)",
                [seq.to_string()],
            )
            .unwrap();
    }

    /// Mark call `id` as variant `variant` of reroll `reroll`, for tests
    /// until rerolls record their own calls.
    #[cfg(test)]
    pub(crate) fn mark_variant(&self, id: i64, reroll: i64, variant: i64) {
        self.conn
            .execute(
                "UPDATE llm_calls SET reroll_id = ?2, variant = ?3 WHERE id = ?1",
                params![id, reroll, variant],
            )
            .unwrap();
    }

    /// The first event that can have call records: events before it were
    /// logged before this save was upgraded to schema 6.
    pub fn recorded_from(&self) -> Result<i64, StoreError> {
        match self.meta("recorded_from")? {
            None => Ok(1),
            Some(v) => v
                .parse()
                .map_err(|_| StoreError::Corrupt(format!("meta recorded_from = {v:?}"))),
        }
    }

    /// The id of the latest recorded call (0 when there are none), so a
    /// session can total only the calls it made with [`World::cost_since`].
    pub fn last_call_id(&self) -> Result<i64, StoreError> {
        Ok(self
            .conn
            .query_row("SELECT COALESCE(MAX(id), 0) FROM llm_calls", [], |r| {
                r.get(0)
            })?)
    }

    /// What the calls recorded after call `id` cost, in US dollars (calls
    /// on models without a known price count as nothing).
    pub fn cost_since(&self, id: i64) -> Result<f64, StoreError> {
        Ok(self.conn.query_row(
            "SELECT COALESCE(SUM(cost_usd), 0.0) FROM llm_calls WHERE id > ?1",
            [id],
            |r| r.get(0),
        )?)
    }

    /// A stored prompt (system prompt or tool definitions) by hash.
    pub fn prompt(&self, hash: &str) -> Result<Option<String>, StoreError> {
        Ok(self
            .conn
            .query_row(
                "SELECT text FROM llm_prompts WHERE hash = ?1",
                [hash],
                |r| r.get(0),
            )
            .optional()?)
    }

    fn query_calls<P: rusqlite::Params>(
        &self,
        filter: &str,
        params: P,
    ) -> Result<Vec<StoredCall>, StoreError> {
        let sql = format!("SELECT {COLUMNS} FROM llm_calls WHERE {filter} ORDER BY id");
        let rows: Vec<RawCall> = self
            .conn
            .prepare(&sql)?
            .query_map(params, RawCall::from_row)?
            .collect::<Result<_, _>>()?;
        rows.into_iter().map(|raw| raw.resolve(self)).collect()
    }
}

/// A row before its JSON is parsed and its prompts looked up.
struct RawCall {
    id: i64,
    turn_seq: Option<i64>,
    reroll_id: Option<i64>,
    variant: Option<i64>,
    log_seq: i64,
    role: String,
    model: String,
    effort: Option<String>,
    system_hash: Option<String>,
    tools_hash: Option<String>,
    request: String,
    response: String,
    usage: [i64; 4],
    latency_ms: i64,
    cost_usd: Option<f64>,
    created_at: String,
}

impl RawCall {
    fn from_row(r: &Row) -> rusqlite::Result<Self> {
        Ok(RawCall {
            id: r.get(0)?,
            turn_seq: r.get(1)?,
            reroll_id: r.get(2)?,
            variant: r.get(3)?,
            log_seq: r.get(4)?,
            role: r.get(5)?,
            model: r.get(6)?,
            effort: r.get(7)?,
            system_hash: r.get(8)?,
            tools_hash: r.get(9)?,
            request: r.get(10)?,
            response: r.get(11)?,
            usage: [r.get(12)?, r.get(13)?, r.get(14)?, r.get(15)?],
            latency_ms: r.get(16)?,
            cost_usd: r.get(17)?,
            created_at: r.get(18)?,
        })
    }

    fn resolve(self, world: &World) -> Result<StoredCall, StoreError> {
        let id = self.id;
        let json = |what: &str, text: &str| -> Result<Value, StoreError> {
            serde_json::from_str(text)
                .map_err(|e| StoreError::Corrupt(format!("llm call {id}: bad {what}: {e}")))
        };
        let prompt = |hash: &Option<String>| -> Result<Option<String>, StoreError> {
            hash.as_deref()
                .map(|h| {
                    world
                        .prompt(h)?
                        .ok_or_else(|| StoreError::Corrupt(format!("llm call {id}: no prompt {h}")))
                })
                .transpose()
        };
        let system = prompt(&self.system_hash)?;
        let tools = prompt(&self.tools_hash)?
            .map(|t| json("tools", &t))
            .transpose()?;
        let [input, output, write, read] = self.usage.map(|n| n as u64);
        Ok(StoredCall {
            id,
            turn_seq: self.turn_seq,
            reroll_id: self.reroll_id,
            variant: self.variant,
            log_seq: self.log_seq,
            created_at: self.created_at,
            call: CallRecord {
                role: self.role,
                model: self.model,
                effort: self.effort,
                system,
                tools,
                request: json("request", &self.request)?,
                response: json("response", &self.response)?,
                usage: Usage {
                    input_tokens: input,
                    output_tokens: output,
                    cache_creation_input_tokens: write,
                    cache_read_input_tokens: read,
                },
                latency_ms: self.latency_ms as u64,
                cost_usd: self.cost_usd,
            },
            system_hash: self.system_hash,
            tools_hash: self.tools_hash,
        })
    }
}

/// Store `text` under its hash unless it's already there; returns the hash.
fn store_prompt(conn: &Connection, text: &str) -> Result<String, StoreError> {
    let hash = prompt_hash(text);
    let existing: Option<String> = conn
        .query_row(
            "SELECT text FROM llm_prompts WHERE hash = ?1",
            [&hash],
            |r| r.get(0),
        )
        .optional()?;
    match existing {
        Some(stored) if stored != text => Err(StoreError::Corrupt(format!(
            "two prompts share hash {hash}"
        ))),
        Some(_) => Ok(hash),
        None => {
            conn.execute(
                "INSERT INTO llm_prompts (hash, text) VALUES (?1, ?2)",
                params![hash, text],
            )?;
            Ok(hash)
        }
    }
}

/// A prompt's hash: 64-bit FNV-1a as hex. Spelled out here rather than
/// taken from the standard library, whose hashers may change between Rust
/// releases while saves keep their hashes forever.
pub(crate) fn prompt_hash(text: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// Copy the main-line call records with `log_seq <= at` (and the prompts
/// they use) from `from` into `to`, keeping their ids. Rerolls' variant
/// calls stay behind: they belong to the save that ran the reroll.
pub(super) fn copy_calls(from: &Connection, to: &Connection, at: i64) -> rusqlite::Result<()> {
    let filter = "log_seq <= ?1 AND reroll_id IS NULL";
    let prompts: Vec<(String, String)> = from
        .prepare(&format!(
            "SELECT hash, text FROM llm_prompts WHERE hash IN (
                SELECT system_hash FROM llm_calls WHERE {filter}
                UNION SELECT tools_hash FROM llm_calls WHERE {filter})"
        ))?
        .query_map([at], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    for (hash, text) in prompts {
        to.execute(
            "INSERT OR IGNORE INTO llm_prompts (hash, text) VALUES (?1, ?2)",
            params![hash, text],
        )?;
    }
    let mut select = from.prepare(&format!(
        "SELECT {COLUMNS} FROM llm_calls WHERE {filter} ORDER BY id"
    ))?;
    let width = select.column_count();
    let placeholders = (1..=width)
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut insert = to.prepare(&format!(
        "INSERT INTO llm_calls ({COLUMNS}) VALUES ({placeholders})"
    ))?;
    let mut rows = select.query([at])?;
    while let Some(row) = rows.next()? {
        let values: Vec<rusqlite::types::Value> =
            (0..width).map(|i| row.get(i)).collect::<Result<_, _>>()?;
        insert.execute(rusqlite::params_from_iter(values))?;
    }
    Ok(())
}
