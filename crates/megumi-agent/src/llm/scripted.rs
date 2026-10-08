//! Deterministic model doubles: one for tests, one for a missing key.
//!
//! [`ScriptedLlm`] replays a fixed list of replies in order and records the
//! requests it saw, so a test can assert both on the reply and on the prompt the
//! pipeline built. [`DisabledLlm`] always fails, standing in when no API key is
//! configured: the agent still ingests and stores every message, it just never
//! speaks.

use std::sync::Mutex;

use super::{BoxFuture, LlmClient, LlmError, LlmRequest, LlmResponse};

/// A model that replays canned replies and records what it was asked.
#[derive(Default)]
pub struct ScriptedLlm {
    replies: Mutex<std::collections::VecDeque<LlmResponse>>,
    requests: Mutex<Vec<LlmRequest>>,
}

impl ScriptedLlm {
    /// A model that will answer with `replies`, in order.
    pub fn new(replies: impl IntoIterator<Item = LlmResponse>) -> Self {
        Self {
            replies: Mutex::new(replies.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
        }
    }

    /// A model that always answers with `text`.
    pub fn always(text: impl Into<String>) -> Self {
        let text = text.into();
        Self::new(std::iter::repeat_n(
            LlmResponse {
                text,
                input_tokens: None,
                output_tokens: None,
            },
            1024,
        ))
    }

    /// Every request the model has been asked, oldest first.
    pub fn requests(&self) -> Vec<LlmRequest> {
        self.requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl LlmClient for ScriptedLlm {
    fn complete(&self, request: LlmRequest) -> BoxFuture<Result<LlmResponse, LlmError>> {
        self.requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(request);
        let reply = self
            .replies
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop_front();
        Box::pin(async move {
            reply.ok_or_else(|| LlmError::Transport("the scripted model ran out of replies".into()))
        })
    }
}

/// A model that is never configured, so every call fails.
///
/// Used when `ANTHROPIC_API_KEY` is unset: the agent keeps storing messages and
/// simply cannot reply. The error is [`LlmError::Disabled`] so a caller can tell
/// "no model" apart from "the model failed".
#[derive(Default)]
pub struct DisabledLlm;

impl LlmClient for DisabledLlm {
    fn complete(&self, _request: LlmRequest) -> BoxFuture<Result<LlmResponse, LlmError>> {
        Box::pin(async { Err(LlmError::Disabled) })
    }
}
