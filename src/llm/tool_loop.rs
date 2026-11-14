//! The request → tool calls → tool results → request loop.

use super::{
    ContentBlock, LlmClient, LlmError, Message, Request, Response, Role, StopReason, ToolUse,
};

/// What a tool handler returns for one call. Errors go back to the model as
/// `is_error` results so it can react (e.g. narrate an impossible move).
#[derive(Debug, Clone, PartialEq)]
pub enum ToolOutput {
    Ok(String),
    Err(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolLoopOutcome {
    /// The conversation including every assistant turn and tool result.
    pub messages: Vec<Message>,
    /// The last model response (its stop reason says why the loop ended).
    pub final_response: Response,
    /// How many model calls the loop made.
    pub calls: usize,
}

/// Send `request`, run every tool the model calls through `handle_tool`, and
/// continue until the model stops asking for tools (end turn, refusal, max
/// tokens…) or `max_calls` model calls have been made.
///
/// Each assistant turn is appended with its full content, thinking blocks
/// included, as the API requires; all results of one turn go back in a single
/// user message.
pub fn run_tool_loop(
    client: &dyn LlmClient,
    mut request: Request,
    max_calls: usize,
    on_text: &mut dyn FnMut(&str),
    handle_tool: &mut dyn FnMut(&ToolUse) -> ToolOutput,
) -> Result<ToolLoopOutcome, LlmError> {
    for calls in 1..=max_calls {
        let response = client.send(&request, on_text)?;
        request
            .messages
            .push(Message::assistant(response.content.clone()));

        if response.stop_reason != Some(StopReason::ToolUse) {
            return Ok(ToolLoopOutcome {
                messages: request.messages,
                final_response: response,
                calls,
            });
        }

        let results: Vec<ContentBlock> = response
            .tool_uses()
            .map(|call| match handle_tool(call) {
                ToolOutput::Ok(out) => ContentBlock::tool_result(&call.id, out, false),
                ToolOutput::Err(err) => ContentBlock::tool_result(&call.id, err, true),
            })
            .collect();
        if results.is_empty() {
            return Err(LlmError::Stream(
                "stop_reason tool_use but no tool_use blocks".into(),
            ));
        }
        request.messages.push(Message {
            role: Role::User,
            content: results,
        });
    }
    Err(LlmError::TooManyIterations(max_calls))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{Model, ScriptedClient, ToolDefinition};
    use serde_json::json;

    fn request() -> Request {
        Request::new(Model::Sonnet, vec![Message::user("kick the rotten door")]).tools(vec![ToolDefinition::new(
            "query_entity",
            "Look up an entity",
            json!({"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]}),
        )])
    }

    #[test]
    fn round_trip_feeds_tool_results_back() {
        let mut first = Response::canned_tool_use("toolu_1", "query_entity", json!({"id": "door"}));
        first.content.insert(
            0,
            ContentBlock::Thinking {
                thinking: String::new(),
                signature: "sig".into(),
            },
        );
        let client = ScriptedClient::new([first, Response::canned_text("The door splinters.")]);

        let mut seen = Vec::new();
        let outcome = run_tool_loop(&client, request(), 5, &mut |_| {}, &mut |call| {
            seen.push((call.name.clone(), call.input.clone()));
            ToolOutput::Ok(r#"{"id":"door","rotten":true}"#.into())
        })
        .unwrap();

        assert_eq!(
            seen,
            vec![("query_entity".to_string(), json!({"id": "door"}))]
        );
        assert_eq!(outcome.calls, 2);
        assert_eq!(outcome.final_response.text(), "The door splinters.");

        let second = &client.requests()[1];
        assert_eq!(second.messages.len(), 3);
        let assistant = &second.messages[1];
        assert_eq!(assistant.role, Role::Assistant);
        assert!(
            matches!(assistant.content[0], ContentBlock::Thinking { .. }),
            "thinking block must be kept"
        );
        assert_eq!(
            second.messages[2].content,
            vec![ContentBlock::tool_result(
                "toolu_1",
                r#"{"id":"door","rotten":true}"#,
                false
            )]
        );
        assert_eq!(outcome.messages.len(), 4);
    }

    #[test]
    fn tool_errors_are_reported_to_the_model() {
        let client = ScriptedClient::new([
            Response::canned_tool_use("toolu_1", "query_entity", json!({"id": "ghost"})),
            Response::canned_text("Nothing there."),
        ]);
        run_tool_loop(&client, request(), 5, &mut |_| {}, &mut |_| {
            ToolOutput::Err("no entity ghost".into())
        })
        .unwrap();
        assert_eq!(
            client.requests()[1].messages[2].content,
            vec![ContentBlock::tool_result(
                "toolu_1",
                "no entity ghost",
                true
            )]
        );
    }

    #[test]
    fn parallel_calls_return_in_one_message() {
        let mut resp = Response::canned_tool_use("a", "query_entity", json!({"id": "x"}));
        resp.content.push(ContentBlock::ToolUse(ToolUse {
            id: "b".into(),
            name: "query_entity".into(),
            input: json!({"id": "y"}),
        }));
        let client = ScriptedClient::new([resp, Response::canned_text("ok")]);
        run_tool_loop(&client, request(), 5, &mut |_| {}, &mut |c| {
            ToolOutput::Ok(c.id.clone())
        })
        .unwrap();
        let results = &client.requests()[1].messages[2].content;
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn stops_at_iteration_cap() {
        let client = ScriptedClient::new((0..3).map(|i| {
            Response::canned_tool_use(format!("t{i}"), "query_entity", json!({"id": "x"}))
        }));
        let err = run_tool_loop(&client, request(), 2, &mut |_| {}, &mut |_| {
            ToolOutput::Ok("x".into())
        })
        .unwrap_err();
        assert!(matches!(err, LlmError::TooManyIterations(2)));
    }

    #[test]
    fn refusal_ends_the_loop() {
        let mut refusal = Response::canned_text("");
        refusal.stop_reason = Some(StopReason::Refusal);
        let client = ScriptedClient::new([refusal]);
        let outcome =
            run_tool_loop(&client, request(), 5, &mut |_| {}, &mut |_| unreachable!()).unwrap();
        assert_eq!(
            outcome.final_response.stop_reason,
            Some(StopReason::Refusal)
        );
    }
}
