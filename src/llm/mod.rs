//! Claude Messages API access for loam.
//!
//! Engine code talks to [`LlmClient`]; production uses [`AnthropicClient`],
//! tests use [`ScriptedClient`]. There is no official Anthropic Rust SDK
//! (checked 2026-11-13), so this is a thin `reqwest` + `serde` client.

mod anthropic;
mod fake;
mod tool_loop;
mod types;

pub use anthropic::AnthropicClient;
pub use fake::ScriptedClient;
pub use tool_loop::{ToolLoopOutcome, ToolOutput, run_tool_loop};
pub use types::*;

/// Something that can answer a Messages API request.
///
/// Every call streams: text deltas are passed to `on_text` as they arrive and
/// the assembled response is returned at the end.
pub trait LlmClient {
    fn send(&self, request: &Request, on_text: &mut dyn FnMut(&str)) -> Result<Response, LlmError>;

    /// [`send`](Self::send) without watching the text stream.
    fn complete(&self, request: &Request) -> Result<Response, LlmError> {
        self.send(request, &mut |_| {})
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("ANTHROPIC_API_KEY is not set")]
    MissingApiKey,
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("API error {status} ({kind}): {message}")]
    Api {
        status: u16,
        kind: String,
        message: String,
    },
    #[error("malformed stream: {0}")]
    Stream(String),
    #[error("scripted client has no response left for request #{0}")]
    ScriptExhausted(usize),
    #[error("tool loop did not finish within {0} model calls")]
    TooManyIterations(usize),
}
