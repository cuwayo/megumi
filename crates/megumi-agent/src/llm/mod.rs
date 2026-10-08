//! Talking to the model, behind a trait so the pipeline is testable.
//!
//! [`LlmClient`] is the one seam between the agent and a model provider. The
//! production implementation is [`AnthropicLlm`], which speaks the Anthropic
//! Messages API over HTTP; [`ScriptedLlm`] replays canned replies for tests and
//! evals; [`DisabledLlm`] stands in when no API key is configured, so the agent
//! still runs and stores messages but never replies.

mod anthropic;
mod scripted;

use std::future::Future;
use std::pin::Pin;

use crate::context::Prompt;

pub use anthropic::AnthropicLlm;
pub use scripted::{DisabledLlm, ScriptedLlm};

/// A boxed future, matching the shape the framework uses for its callbacks.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// One request to the model.
#[derive(Clone, Debug)]
pub struct LlmRequest {
    /// The model id to call.
    pub model: String,
    /// The stable system prompt.
    pub system: String,
    /// The volatile user turn.
    pub user: String,
    /// The cap on reply tokens.
    pub max_tokens: u32,
}

impl LlmRequest {
    /// Builds a request from an assembled [`Prompt`] and the run's settings.
    pub fn from_prompt(prompt: &Prompt, model: &str, max_tokens: u32) -> Self {
        Self {
            model: model.to_string(),
            system: prompt.system.clone(),
            user: prompt.user.clone(),
            max_tokens,
        }
    }
}

/// One reply from the model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LlmResponse {
    /// The reply text, before the agent strips a `NO_REPLY`.
    pub text: String,
    /// Input tokens the model reported, when it reported any.
    pub input_tokens: Option<u32>,
    /// Output tokens the model reported, when it reported any.
    pub output_tokens: Option<u32>,
}

/// Why a model call failed.
#[derive(Debug)]
pub enum LlmError {
    /// The request could not be sent, or the response could not be read.
    Transport(String),
    /// The provider returned a non-success status.
    Status {
        /// The HTTP status code.
        status: u16,
        /// The provider's message, trimmed.
        body: String,
    },
    /// The provider returned a body this client could not parse.
    Decode(String),
    /// The provider answered with an error, even on a success status.
    Provider(String),
    /// No model is configured, so the agent cannot reply.
    Disabled,
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(message) => write!(f, "the model request failed: {message}"),
            Self::Status { status, body } => {
                write!(f, "the model returned HTTP {status}: {body}")
            }
            Self::Decode(message) => write!(f, "the model response could not be read: {message}"),
            Self::Provider(message) => write!(f, "the model provider returned an error: {message}"),
            Self::Disabled => write!(f, "no model is configured"),
        }
    }
}

impl std::error::Error for LlmError {}

/// A model the agent can ask for a completion.
///
/// Implementations are shared across turns, so they take `&self` and must be
/// safe to call concurrently.
pub trait LlmClient: Send + Sync {
    /// Asks the model to complete `request`.
    fn complete(&self, request: LlmRequest) -> BoxFuture<Result<LlmResponse, LlmError>>;
}
