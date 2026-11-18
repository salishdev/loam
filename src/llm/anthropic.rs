//! The real client: `POST /v1/messages` with `stream: true`, parsed from SSE.

use std::io::BufRead;
use std::thread;
use std::time::Duration;

use serde_json::{Map, Value, json};

use super::{ContentBlock, LlmClient, LlmError, Request, Response, StopReason, Usage};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const API_VERSION: &str = "2026-10-21";
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
const MAX_RETRIES: u32 = 2;

pub struct AnthropicClient {
    http: reqwest::blocking::Client,
    api_key: String,
    base_url: String,
}

impl AnthropicClient {
    pub fn new(api_key: impl Into<String>) -> Result<Self, LlmError> {
        let http = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            // Whole-response cap; long director turns stream for a while.
            .timeout(Duration::from_secs(600))
            .build()?;
        Ok(AnthropicClient {
            http,
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.into(),
        })
    }

    /// Reads `ANTHROPIC_API_KEY`, and `ANTHROPIC_BASE_URL` if set.
    pub fn from_env() -> Result<Self, LlmError> {
        let key = std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .filter(|k| !k.is_empty())
            .ok_or(LlmError::MissingApiKey)?;
        let mut client = Self::new(key)?;
        if let Ok(url) = std::env::var("ANTHROPIC_BASE_URL")
            && !url.is_empty()
        {
            client.base_url = url.trim_end_matches('/').to_string();
        }
        Ok(client)
    }

    fn post(&self, request: &Request) -> Result<reqwest::blocking::Response, LlmError> {
        let body = request_body(request);
        let mut attempt = 0;
        loop {
            let mut builder = self
                .http
                .post(format!("{}/v1/messages", self.base_url))
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", API_VERSION)
                .json(&body);
            if request.model.supports_server_fallback() {
                builder = builder.header("anthropic-beta", FALLBACK_BETA);
            }
            let resp = builder.send()?;
            let status = resp.status().as_u16();
            if resp.status().is_success() {
                return Ok(resp);
            }
            let retryable = status == 429 || status >= 500;
            if retryable && attempt < MAX_RETRIES {
                let wait = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .map(Duration::from_secs)
                    .unwrap_or(Duration::from_secs(1 << attempt));
                attempt += 1;
                thread::sleep(wait.min(Duration::from_secs(30)));
                continue;
            }
            let text = resp.text().unwrap_or_default();
            return Err(api_error(status, &text));
        }
    }
}

impl LlmClient for AnthropicClient {
    fn send(&self, request: &Request, on_text: &mut dyn FnMut(&str)) -> Result<Response, LlmError> {
        let resp = self.post(request)?;
        parse_stream(std::io::BufReader::new(resp), on_text)
    }
}

/// The wire body. Caching: an explicit breakpoint on the system prompt
/// (render order is tools → system → messages, so it caches both), plus
/// top-level automatic caching, whose breakpoint follows the end of the
/// conversation so each call in a tool loop reads back the turns before it.
/// Prefixes under the model's minimum (512 tokens on the 5.5 models) silently
/// don't cache.
fn request_body(request: &Request) -> Value {
    let mut body = json!({
        "model": request.model.id(),
        "max_tokens": request.max_tokens,
        "stream": true,
        "cache_control": { "type": "ephemeral" },
        "messages": request.messages,
    });
    let obj = body.as_object_mut().expect("object literal");
    if let Some(system) = &request.system {
        obj.insert(
            "system".into(),
            json!([{
                "type": "text",
                "text": system,
                "cache_control": { "type": "ephemeral" },
            }]),
        );
    }
    if !request.tools.is_empty() {
        obj.insert("tools".into(), json!(request.tools));
    }
    if let Some(effort) = request.effort {
        obj.insert("output_config".into(), json!({ "effort": effort }));
    }
    if request.model.supports_server_fallback() {
        obj.insert("fallbacks".into(), json!("default"));
    }
    body
}

fn api_error(status: u16, body: &str) -> LlmError {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let err = parsed.as_ref().and_then(|v| v.get("error"));
    let field = |k: &str| {
        err.and_then(|e| e.get(k))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    LlmError::Api {
        status,
        kind: field("type").unwrap_or_else(|| "unknown".into()),
        message: field("message").unwrap_or_else(|| body.to_string()),
    }
}

/// A content block being assembled from stream deltas: the block from
/// `content_block_start` plus any tool-input JSON fragments.
struct PartialBlock {
    block: Map<String, Value>,
    input_json: String,
}

impl PartialBlock {
    fn append(&mut self, key: &str, piece: &str) {
        let slot = self
            .block
            .entry(key)
            .or_insert_with(|| Value::String(String::new()));
        if let Value::String(s) = slot {
            s.push_str(piece);
        }
    }

    fn finish(mut self) -> Result<ContentBlock, LlmError> {
        if !self.input_json.is_empty() {
            let input: Value = serde_json::from_str(&self.input_json)
                .map_err(|e| LlmError::Stream(format!("tool input is not valid JSON: {e}")))?;
            self.block.insert("input".into(), input);
        }
        serde_json::from_value(Value::Object(self.block))
            .map_err(|e| LlmError::Stream(format!("bad content block: {e}")))
    }
}

/// Parse a Messages API SSE stream into a [`Response`], passing text deltas to
/// `on_text` as they arrive.
pub(crate) fn parse_stream(
    reader: impl BufRead,
    on_text: &mut dyn FnMut(&str),
) -> Result<Response, LlmError> {
    let mut response: Option<Response> = None;
    let mut blocks: Vec<Option<PartialBlock>> = Vec::new();
    let mut data = String::new();
    let mut stopped = false;

    for line in reader.lines() {
        let line = line.map_err(|e| LlmError::Stream(format!("read failed: {e}")))?;
        if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        } else if line.is_empty() && !data.is_empty() {
            stopped = dispatch(&mut data, &mut response, &mut blocks, on_text)?;
            if stopped {
                break;
            }
        }
        // `event:` lines duplicate the JSON `type`; comments are ignored.
    }
    if !stopped && !data.is_empty() {
        stopped = dispatch(&mut data, &mut response, &mut blocks, on_text)?;
    }

    let response =
        response.ok_or_else(|| LlmError::Stream("stream ended before message_start".into()))?;
    if !stopped {
        return Err(LlmError::Stream("stream ended before message_stop".into()));
    }
    if let Some(i) = blocks.iter().position(Option::is_some) {
        return Err(LlmError::Stream(format!("content block {i} never stopped")));
    }
    Ok(response)
}

fn dispatch(
    data: &mut String,
    response: &mut Option<Response>,
    blocks: &mut Vec<Option<PartialBlock>>,
    on_text: &mut dyn FnMut(&str),
) -> Result<bool, LlmError> {
    let event: Value = serde_json::from_str(data)
        .map_err(|e| LlmError::Stream(format!("bad event JSON: {e}: {data}")))?;
    data.clear();
    handle_event(&event, response, blocks, on_text)
}

/// Apply one event. Returns true on `message_stop`.
fn handle_event(
    event: &Value,
    response: &mut Option<Response>,
    blocks: &mut Vec<Option<PartialBlock>>,
    on_text: &mut dyn FnMut(&str),
) -> Result<bool, LlmError> {
    let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
    let index = || {
        event
            .get("index")
            .and_then(Value::as_u64)
            .map(|i| i as usize)
    };
    let need_response = |r: &mut Option<Response>| -> Result<(), LlmError> {
        if r.is_none() {
            Err(LlmError::Stream(format!("{kind} before message_start")))
        } else {
            Ok(())
        }
    };

    match kind {
        "message_start" => {
            let msg = &event["message"];
            *response = Some(Response {
                id: msg["id"].as_str().unwrap_or_default().to_string(),
                model: msg["model"].as_str().unwrap_or_default().to_string(),
                content: Vec::new(),
                stop_reason: None,
                usage: serde_json::from_value(msg["usage"].clone()).unwrap_or_default(),
            });
        }
        "content_block_start" => {
            need_response(response)?;
            let i = index()
                .ok_or_else(|| LlmError::Stream("content_block_start without index".into()))?;
            let block = event["content_block"]
                .as_object()
                .cloned()
                .ok_or_else(|| LlmError::Stream("content_block_start without block".into()))?;
            if blocks.len() <= i {
                blocks.resize_with(i + 1, || None);
            }
            blocks[i] = Some(PartialBlock {
                block,
                input_json: String::new(),
            });
        }
        "content_block_delta" => {
            let i = index().unwrap_or(usize::MAX);
            let partial = blocks
                .get_mut(i)
                .and_then(Option::as_mut)
                .ok_or_else(|| LlmError::Stream(format!("delta for unknown block {i}")))?;
            let delta = &event["delta"];
            let s = |k: &str| delta[k].as_str().unwrap_or_default();
            match delta["type"].as_str().unwrap_or("") {
                "text_delta" => {
                    on_text(s("text"));
                    partial.append("text", s("text"));
                }
                "input_json_delta" => partial.input_json.push_str(s("partial_json")),
                "thinking_delta" => partial.append("thinking", s("thinking")),
                "signature_delta" => partial.append("signature", s("signature")),
                _ => {} // e.g. citations; not used by loam
            }
        }
        "content_block_stop" => {
            let i = index().unwrap_or(usize::MAX);
            let partial = blocks
                .get_mut(i)
                .and_then(Option::take)
                .ok_or_else(|| LlmError::Stream(format!("stop for unknown block {i}")))?;
            let block = partial.finish()?;
            let resp = response
                .as_mut()
                .ok_or_else(|| LlmError::Stream("block before message_start".into()))?;
            // Blocks are streamed one at a time, in index order.
            resp.content.push(block);
        }
        "message_delta" => {
            need_response(response)?;
            let resp = response.as_mut().expect("checked");
            if let Some(reason) = event["delta"].get("stop_reason").filter(|v| !v.is_null()) {
                resp.stop_reason = Some(
                    serde_json::from_value::<StopReason>(reason.clone())
                        .map_err(|e| LlmError::Stream(format!("bad stop_reason: {e}")))?,
                );
            }
            if let Some(usage) = event.get("usage") {
                // Counts here are cumulative; input and cache fields may be
                // absent (zero), in which case message_start's stand.
                let delta: Usage = serde_json::from_value(usage.clone()).unwrap_or_default();
                let u = &mut resp.usage;
                u.output_tokens = delta.output_tokens;
                for (slot, new) in [
                    (&mut u.input_tokens, delta.input_tokens),
                    (
                        &mut u.cache_creation_input_tokens,
                        delta.cache_creation_input_tokens,
                    ),
                    (
                        &mut u.cache_read_input_tokens,
                        delta.cache_read_input_tokens,
                    ),
                ] {
                    if new > 0 {
                        *slot = new;
                    }
                }
            }
        }
        "message_stop" => {
            need_response(response)?;
            return Ok(true);
        }
        "error" => {
            let err = &event["error"];
            return Err(LlmError::Api {
                status: 200,
                kind: err["type"].as_str().unwrap_or("unknown").to_string(),
                message: err["message"].as_str().unwrap_or_default().to_string(),
            });
        }
        _ => {} // ping and future event types
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{Effort, Message, Model, ToolDefinition, ToolUse};

    fn sse(events: &[Value]) -> String {
        events
            .iter()
            .map(|e| format!("event: {}\ndata: {}\n\n", e["type"].as_str().unwrap(), e))
            .collect()
    }

    fn start() -> Value {
        json!({"type": "message_start", "message": {"id": "msg_1", "model": "claude-haiku-5-5", "usage": {"input_tokens": 42, "output_tokens": 1}}})
    }

    fn end(reason: &str) -> [Value; 2] {
        [
            json!({"type": "message_delta", "delta": {"stop_reason": reason}, "usage": {"output_tokens": 17}}),
            json!({"type": "message_stop"}),
        ]
    }

    fn parse(events: &[Value]) -> (Result<Response, LlmError>, String) {
        let body = sse(events);
        let mut text = String::new();
        let r = parse_stream(body.as_bytes(), &mut |t| text.push_str(t));
        (r, text)
    }

    #[test]
    fn streams_text_and_assembles_response() {
        let mut events = vec![
            start(),
            json!({"type": "ping"}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "The crow "}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "listens."}}),
            json!({"type": "content_block_stop", "index": 0}),
        ];
        events.extend(end("end_turn"));
        let (resp, streamed) = parse(&events);
        let resp = resp.unwrap();
        assert_eq!(streamed, "The crow listens.");
        assert_eq!(resp.text(), "The crow listens.");
        assert_eq!(resp.stop_reason, Some(StopReason::EndTurn));
        assert_eq!(resp.usage.input_tokens, 42);
        assert_eq!(resp.usage.output_tokens, 17);
        assert_eq!(resp.id, "msg_1");
    }

    #[test]
    fn assembles_thinking_and_tool_use() {
        let mut events = vec![
            start(),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "abc"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "toolu_9", "name": "take", "input": {}}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"item\": \"la"}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "ntern\"}"}}),
            json!({"type": "content_block_stop", "index": 1}),
        ];
        events.extend(end("tool_use"));
        let resp = parse(&events).0.unwrap();
        assert_eq!(
            resp.content[0],
            ContentBlock::Thinking {
                thinking: String::new(),
                signature: "abc".into()
            }
        );
        assert_eq!(
            resp.tool_uses().collect::<Vec<_>>(),
            vec![&ToolUse {
                id: "toolu_9".into(),
                name: "take".into(),
                input: json!({"item": "lantern"})
            }]
        );
        assert_eq!(resp.stop_reason, Some(StopReason::ToolUse));
    }

    #[test]
    fn tool_with_no_input_keeps_empty_object() {
        let mut events = vec![
            start(),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "t", "name": "inventory_list", "input": {}}}),
            json!({"type": "content_block_stop", "index": 0}),
        ];
        events.extend(end("tool_use"));
        let resp = parse(&events).0.unwrap();
        assert_eq!(resp.tool_uses().next().unwrap().input, json!({}));
    }

    #[test]
    fn unknown_blocks_are_kept() {
        let mut events = vec![
            start(),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "fallback", "from": "x"}}),
            json!({"type": "content_block_stop", "index": 0}),
        ];
        events.extend(end("end_turn"));
        let resp = parse(&events).0.unwrap();
        assert_eq!(
            resp.content,
            vec![ContentBlock::Other(
                json!({"type": "fallback", "from": "x"})
            )]
        );
    }

    #[test]
    fn error_event_becomes_api_error() {
        let events = vec![
            start(),
            json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}),
        ];
        match parse(&events).0 {
            Err(LlmError::Api { kind, message, .. }) => {
                assert_eq!(kind, "overloaded_error");
                assert_eq!(message, "Overloaded");
            }
            other => panic!("expected API error, got {other:?}"),
        }
    }

    #[test]
    fn truncated_stream_is_an_error() {
        let events = vec![
            start(),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        ];
        assert!(matches!(parse(&events).0, Err(LlmError::Stream(_))));
    }

    #[test]
    fn bad_tool_json_is_an_error() {
        let mut events = vec![
            start(),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "t", "name": "x", "input": {}}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"a\":"}}),
            json!({"type": "content_block_stop", "index": 0}),
        ];
        events.extend(end("tool_use"));
        assert!(matches!(parse(&events).0, Err(LlmError::Stream(_))));
    }

    #[test]
    fn request_body_shape() {
        let req = Request::new(Model::Haiku, vec![Message::user("hi")])
            .system("be terse")
            .effort(Effort::Low)
            .tools(vec![ToolDefinition::new(
                "look",
                "Look around",
                json!({"type": "object"}),
            )]);
        let body = request_body(&req);
        assert_eq!(body["model"], "claude-haiku-5-5");
        assert_eq!(body["stream"], true);
        assert_eq!(
            body["system"],
            json!([{"type": "text", "text": "be terse", "cache_control": {"type": "ephemeral"}}])
        );
        assert_eq!(body["cache_control"], json!({"type": "ephemeral"}));
        assert_eq!(body["output_config"], json!({"effort": "low"}));
        assert_eq!(body["tools"][0]["name"], "look");
        assert_eq!(
            body["messages"][0],
            json!({"role": "user", "content": [{"type": "text", "text": "hi"}]})
        );
        assert!(body.get("fallbacks").is_none());

        let sonnet = request_body(&Request::new(Model::Sonnet, vec![Message::user("hi")]));
        assert_eq!(sonnet["fallbacks"], "default");
        assert!(sonnet.get("tools").is_none() && sonnet.get("system").is_none());
        assert_eq!(sonnet["cache_control"], json!({"type": "ephemeral"}));

        let opus =
            request_body(&Request::new(Model::Opus, vec![Message::user("hi")]).effort(Effort::Max));
        assert_eq!(opus["model"], "claude-opus-5-5");
        assert_eq!(opus["fallbacks"], "default");
        assert_eq!(opus["output_config"], json!({"effort": "max"}));
    }

    #[test]
    fn cache_usage_is_parsed_and_merged() {
        let events = vec![
            json!({"type": "message_start", "message": {"id": "m", "model": "x", "usage": {
                "input_tokens": 5, "output_tokens": 1,
                "cache_creation_input_tokens": 30, "cache_read_input_tokens": 900}}}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {
                "output_tokens": 12, "cache_creation_input_tokens": 31}}),
            json!({"type": "message_stop"}),
        ];
        let usage = parse(&events).0.unwrap().usage;
        assert_eq!(usage.input_tokens, 5);
        assert_eq!(usage.output_tokens, 12);
        assert_eq!(usage.cache_creation_input_tokens, 31);
        assert_eq!(usage.cache_read_input_tokens, 900);
    }

    #[test]
    fn api_error_parses_body() {
        let e = api_error(
            400,
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad"}}"#,
        );
        assert!(
            matches!(e, LlmError::Api { status: 400, ref kind, ref message } if kind == "invalid_request_error" && message == "bad")
        );
        let e = api_error(502, "gateway");
        assert!(matches!(e, LlmError::Api { ref message, .. } if message == "gateway"));
    }
}
