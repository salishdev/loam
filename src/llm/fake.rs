//! A scripted [`LlmClient`] for tests: no network, canned responses in order.

use std::collections::VecDeque;
use std::sync::Mutex;

use super::{ContentBlock, LlmClient, LlmError, Request, Response};

/// Returns queued responses one per call and records every request it saw.
///
/// Text blocks are fed to `on_text` word by word, so streaming consumers are
/// exercised the same way the real client exercises them.
#[derive(Debug, Default)]
pub struct ScriptedClient {
    script: Mutex<VecDeque<Response>>,
    requests: Mutex<Vec<Request>>,
}

impl ScriptedClient {
    pub fn new(responses: impl IntoIterator<Item = Response>) -> Self {
        ScriptedClient {
            script: Mutex::new(responses.into_iter().collect()),
            requests: Mutex::default(),
        }
    }

    /// Queue another response.
    pub fn push(&self, response: Response) {
        self.script.lock().unwrap().push_back(response);
    }

    /// Every request received so far, in order.
    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

    /// Responses not yet consumed.
    pub fn remaining(&self) -> usize {
        self.script.lock().unwrap().len()
    }
}

impl LlmClient for ScriptedClient {
    fn send(&self, request: &Request, on_text: &mut dyn FnMut(&str)) -> Result<Response, LlmError> {
        let n = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(request.clone());
            requests.len()
        };
        let response = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .ok_or(LlmError::ScriptExhausted(n))?;
        for block in &response.content {
            if let ContentBlock::Text { text } = block {
                for piece in text.split_inclusive(' ') {
                    on_text(piece);
                }
            }
        }
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{Message, Model};

    #[test]
    fn replays_script_and_streams_text() {
        let client = ScriptedClient::new([Response::canned_text("the door creaks open")]);
        let req = Request::new(Model::Haiku, vec![Message::user("open door")]);
        let mut streamed = Vec::new();
        let resp = client
            .send(&req, &mut |t| streamed.push(t.to_string()))
            .unwrap();
        assert_eq!(resp.text(), "the door creaks open");
        assert_eq!(streamed.concat(), "the door creaks open");
        assert!(streamed.len() > 1, "text should arrive in pieces");
        assert_eq!(client.requests(), vec![req]);
    }

    #[test]
    fn errors_when_script_runs_out() {
        let client = ScriptedClient::default();
        let req = Request::new(Model::Haiku, vec![Message::user("hi")]);
        assert!(matches!(
            client.complete(&req),
            Err(LlmError::ScriptExhausted(1))
        ));
    }
}
