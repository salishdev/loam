//! Live Claude API checks. They cost money and need `ANTHROPIC_API_KEY`, so
//! they are ignored by default:
//!
//! ```sh
//! ANTHROPIC_API_KEY=... cargo test --test live_api -- --ignored --nocapture
//! ```

use loam::llm::{
    AnthropicClient, Message, Model, Request, StopReason, ToolDefinition, ToolOutput, run_tool_loop,
};
use serde_json::json;

fn round_trip(model: Model) {
    let client = AnthropicClient::from_env().expect("set ANTHROPIC_API_KEY to run live tests");
    let secret = "7319";
    let request = Request::new(
        model,
        vec![Message::user(
            "What is the secret number? Use the tool, then reply with just the number.",
        )],
    )
    .max_tokens(2_000)
    .tools(vec![ToolDefinition::new(
        "get_secret_number",
        "Returns the secret number. Takes no arguments.",
        json!({"type": "object", "properties": {}, "additionalProperties": false}),
    )]);

    let mut tool_calls = 0;
    let mut streamed = String::new();
    let outcome = run_tool_loop(
        &client,
        request,
        4,
        &mut |t| streamed.push_str(t),
        &mut |call| {
            assert_eq!(call.name, "get_secret_number");
            tool_calls += 1;
            ToolOutput::Ok(secret.into())
        },
    )
    .expect("tool loop should succeed");

    println!(
        "model={} calls={} text={:?} usage={:?}",
        outcome.final_response.model,
        outcome.calls,
        outcome.final_response.text(),
        outcome.final_response.usage
    );
    assert!(tool_calls >= 1, "model never called the tool");
    assert_eq!(
        outcome.final_response.stop_reason,
        Some(StopReason::EndTurn)
    );
    assert!(outcome.final_response.text().contains(secret));
    assert!(
        streamed.contains(secret),
        "answer should have arrived through the text stream"
    );
}

#[test]
#[ignore = "calls the live Claude API; needs ANTHROPIC_API_KEY"]
fn haiku_tool_use_round_trip() {
    round_trip(Model::Haiku);
}

#[test]
#[ignore = "calls the live Claude API; needs ANTHROPIC_API_KEY"]
fn sonnet_tool_use_round_trip() {
    round_trip(Model::Sonnet);
}
