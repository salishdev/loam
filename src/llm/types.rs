//! Wire types for the Claude Messages API, trimmed to what loam uses.
//!
//! Content blocks the engine doesn't model (fallback markers, new block types)
//! deserialize into [`ContentBlock::Other`] and serialize back unchanged, so a
//! multi-turn tool loop never drops something the API expects to see again.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Which Claude model a request goes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Model {
    /// Fast, cheap: interpretation, rule matching, quest judging.
    Haiku,
    /// Director and narrator.
    Sonnet,
    /// Any other model ID, passed through as-is.
    Custom(String),
}

impl Model {
    pub fn id(&self) -> &str {
        match self {
            Model::Haiku => "claude-haiku-5-5",
            Model::Sonnet => "claude-sonnet-5-5",
            Model::Custom(id) => id,
        }
    }

    /// Whether the API offers server-side refusal fallback for this model.
    pub(crate) fn supports_server_fallback(&self) -> bool {
        matches!(self, Model::Sonnet)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Message {
            role: Role::User,
            content: vec![ContentBlock::text(text)],
        }
    }

    pub fn assistant(content: Vec<ContentBlock>) -> Self {
        Message {
            role: Role::Assistant,
            content,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse(ToolUse),
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
    },
    /// Model reasoning. Must be sent back unchanged in later turns of a tool
    /// loop; `thinking` is often empty because the API omits it by default.
    Thinking {
        thinking: String,
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
    /// Any block type loam doesn't model, kept verbatim.
    #[serde(untagged)]
    Other(Value),
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        ContentBlock::Text { text: text.into() }
    }

    pub fn tool_result(
        tool_use_id: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Self {
        ContentBlock::ToolResult {
            tool_use_id: tool_use_id.into(),
            content: content.into(),
            is_error,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolUse {
    pub id: String,
    pub name: String,
    pub input: Value,
}

/// A client tool the model may call. `input_schema` is a JSON Schema object.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

impl ToolDefinition {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Self {
        ToolDefinition {
            name: name.into(),
            description: description.into(),
            input_schema,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
}

/// One Messages API call. Thinking is left at the model default (adaptive).
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub model: Model,
    pub max_tokens: u32,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub effort: Option<Effort>,
}

impl Request {
    pub fn new(model: Model, messages: Vec<Message>) -> Self {
        Request {
            model,
            max_tokens: 16_000,
            system: None,
            messages,
            tools: Vec::new(),
            effort: None,
        }
    }

    pub fn system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }

    pub fn tools(mut self, tools: Vec<ToolDefinition>) -> Self {
        self.tools = tools;
        self
    }

    pub fn max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    pub fn effort(mut self, effort: Effort) -> Self {
        self.effort = Some(effort);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    PauseTurn,
    Refusal,
    #[serde(untagged)]
    Other(String),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    pub id: String,
    /// The model that actually answered (may differ after a refusal fallback).
    pub model: String,
    pub content: Vec<ContentBlock>,
    pub stop_reason: Option<StopReason>,
    pub usage: Usage,
}

impl Response {
    /// Concatenated text of all text blocks.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    pub fn tool_uses(&self) -> impl Iterator<Item = &ToolUse> {
        self.content.iter().filter_map(|b| match b {
            ContentBlock::ToolUse(t) => Some(t),
            _ => None,
        })
    }

    /// A canned end-turn text reply, for scripted fakes.
    pub fn canned_text(text: impl Into<String>) -> Self {
        Response {
            id: "msg_fake".into(),
            model: "fake".into(),
            content: vec![ContentBlock::text(text)],
            stop_reason: Some(StopReason::EndTurn),
            usage: Usage::default(),
        }
    }

    /// A canned tool-use reply, for scripted fakes.
    pub fn canned_tool_use(id: impl Into<String>, name: impl Into<String>, input: Value) -> Self {
        Response {
            id: "msg_fake".into(),
            model: "fake".into(),
            content: vec![ContentBlock::ToolUse(ToolUse {
                id: id.into(),
                name: name.into(),
                input,
            })],
            stop_reason: Some(StopReason::ToolUse),
            usage: Usage::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unknown_blocks_round_trip_verbatim() {
        let raw = json!({"type": "fallback", "model": "claude-sonnet-5", "extra": [1, 2]});
        let block: ContentBlock = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(block, ContentBlock::Other(raw.clone()));
        assert_eq!(serde_json::to_value(&block).unwrap(), raw);
    }

    #[test]
    fn known_blocks_serialize_in_wire_shape() {
        let tool = ContentBlock::ToolUse(ToolUse {
            id: "t1".into(),
            name: "look".into(),
            input: json!({}),
        });
        assert_eq!(
            serde_json::to_value(&tool).unwrap(),
            json!({"type": "tool_use", "id": "t1", "name": "look", "input": {}})
        );
        let ok = ContentBlock::tool_result("t1", "fine", false);
        assert_eq!(
            serde_json::to_value(&ok).unwrap(),
            json!({"type": "tool_result", "tool_use_id": "t1", "content": "fine"})
        );
        let err = ContentBlock::tool_result("t1", "nope", true);
        assert_eq!(serde_json::to_value(&err).unwrap()["is_error"], json!(true));
    }

    #[test]
    fn stop_reason_parses_known_and_unknown() {
        let r: StopReason = serde_json::from_value(json!("refusal")).unwrap();
        assert_eq!(r, StopReason::Refusal);
        let r: StopReason = serde_json::from_value(json!("something_new")).unwrap();
        assert_eq!(r, StopReason::Other("something_new".into()));
    }
}
