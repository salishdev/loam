//! Recording every model call: a decorator over [`LlmClient`].

use std::cell::RefCell;
use std::time::Instant;

use serde_json::{Value, json};

use super::{LlmClient, LlmError, Request, Response, Usage, cost_usd};

/// One finished model call, ready to store.
///
/// The system prompt and tool definitions are kept apart from the rest of
/// the request because they repeat on every call; the store keeps one copy
/// of each by hash.
#[derive(Debug, Clone, PartialEq)]
pub struct CallRecord {
    /// What the call was for (`director`, `interpret`, …), from
    /// [`Request::role`].
    pub role: String,
    /// The requested model id.
    pub model: String,
    pub effort: Option<String>,
    pub system: Option<String>,
    /// The tool definitions as sent, or `None` when the call had none.
    pub tools: Option<Value>,
    /// The request without its system prompt or tool definitions (their
    /// names are kept under `tool_names`).
    pub request: Value,
    pub response: Value,
    pub usage: Usage,
    pub latency_ms: u64,
    /// `None` when the model isn't in the price table.
    pub cost_usd: Option<f64>,
}

/// Role recorded for a request that didn't name one.
pub const UNLABELLED: &str = "other";

impl CallRecord {
    pub fn new(request: &Request, response: &Response, latency_ms: u64) -> Self {
        let model = request.model.id().to_owned();
        let mut req = json!({
            "model": model,
            "max_tokens": request.max_tokens,
            "messages": request.messages,
        });
        if let Some(effort) = request.effort {
            req["effort"] = json!(effort);
        }
        if !request.tools.is_empty() {
            req["tool_names"] = json!(request.tools.iter().map(|t| &t.name).collect::<Vec<_>>());
        }
        CallRecord {
            role: request.role.unwrap_or(UNLABELLED).to_owned(),
            cost_usd: cost_usd(&model, &response.usage),
            model,
            effort: request.effort.map(|e| e.as_str().to_owned()),
            system: request.system.clone(),
            tools: (!request.tools.is_empty()).then(|| json!(request.tools)),
            request: req,
            response: json!(response),
            usage: response.usage,
            latency_ms,
        }
    }
}

/// Wraps a client and keeps a [`CallRecord`] of every call that returns a
/// response. A streamed call is recorded when its stream finishes. Failed
/// calls leave no record.
pub struct RecordingClient<'a> {
    inner: &'a dyn LlmClient,
    calls: RefCell<Vec<CallRecord>>,
}

impl<'a> RecordingClient<'a> {
    pub fn new(inner: &'a dyn LlmClient) -> Self {
        RecordingClient {
            inner,
            calls: RefCell::default(),
        }
    }

    /// The records kept so far, in call order, leaving none behind.
    pub fn take(&self) -> Vec<CallRecord> {
        std::mem::take(&mut *self.calls.borrow_mut())
    }
}

impl LlmClient for RecordingClient<'_> {
    fn send(&self, request: &Request, on_text: &mut dyn FnMut(&str)) -> Result<Response, LlmError> {
        let started = Instant::now();
        let response = self.inner.send(request, on_text)?;
        let latency_ms = started.elapsed().as_millis() as u64;
        self.calls
            .borrow_mut()
            .push(CallRecord::new(request, &response, latency_ms));
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{Effort, Message, Model, ScriptedClient, ToolDefinition};

    #[test]
    fn records_each_call_with_role_usage_and_cost() {
        let mut reply = Response::canned_text("The heron watches.");
        reply.usage = Usage {
            input_tokens: 100,
            output_tokens: 20,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 1000,
        };
        let inner = ScriptedClient::new([reply, Response::canned_text("x")]);
        let client = RecordingClient::new(&inner);
        let request = Request::new(Model::Sonnet, vec![Message::user("look")])
            .system("You are the director.")
            .effort(Effort::Medium)
            .tools(vec![ToolDefinition::new(
                "look",
                "Look",
                json!({"type": "object"}),
            )])
            .role("director");
        let mut streamed = String::new();
        client
            .send(&request, &mut |t| streamed.push_str(t))
            .unwrap();
        assert_eq!(streamed, "The heron watches.");
        client
            .complete(&Request::new(Model::Haiku, vec![Message::user("hi")]))
            .unwrap();

        let calls = client.take();
        assert_eq!(calls.len(), 2);
        let c = &calls[0];
        assert_eq!(c.role, "director");
        assert_eq!(c.model, "claude-sonnet-5-5");
        assert_eq!(c.effort.as_deref(), Some("medium"));
        assert_eq!(c.system.as_deref(), Some("You are the director."));
        assert_eq!(c.tools.as_ref().unwrap()[0]["name"], "look");
        assert!(c.request.get("system").is_none());
        assert_eq!(c.request["tool_names"], json!(["look"]));
        assert_eq!(c.request["messages"][0]["content"][0]["text"], "look");
        assert_eq!(c.response["content"][0]["text"], "The heron watches.");
        assert_eq!(c.response["stop_reason"], "end_turn");
        assert_eq!(c.usage.cache_read_input_tokens, 1000);
        let cost = c.cost_usd.unwrap();
        assert!((cost - (0.0002 + 0.0002 + 0.0001)).abs() < 1e-12, "{cost}");
        assert_eq!(calls[1].role, UNLABELLED);
        assert!(client.take().is_empty());
    }

    #[test]
    fn failed_calls_leave_no_record() {
        let inner = ScriptedClient::default();
        let client = RecordingClient::new(&inner);
        let request = Request::new(Model::Haiku, vec![Message::user("hi")]);
        assert!(client.complete(&request).is_err());
        assert!(client.take().is_empty());
    }
}
