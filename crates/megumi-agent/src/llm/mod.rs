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
    ///
    /// Kept alongside [`messages`](Self::messages) because it is the first user
    /// turn and the single-call paths — extraction, and a turn with no tools —
    /// send it on its own. A tool loop sends `messages` instead, whose first
    /// entry is this same text.
    pub user: String,
    /// The cap on reply tokens.
    pub max_tokens: u32,
    /// The tools the model may call, empty when it may call none.
    pub tools: Vec<ToolSpec>,
    /// The full conversation to send, in order.
    ///
    /// Empty means "just [`user`](Self::user)" — the single-call shape every
    /// pre-tool caller uses. A tool loop fills this so the model sees its own
    /// tool calls and their results.
    pub messages: Vec<LlmMessage>,
}

impl LlmRequest {
    /// Builds a request from an assembled [`Prompt`] and the run's settings.
    pub fn from_prompt(prompt: &Prompt, model: &str, max_tokens: u32) -> Self {
        Self {
            model: model.to_string(),
            system: prompt.system.clone(),
            user: prompt.user.clone(),
            max_tokens,
            tools: Vec::new(),
            messages: Vec::new(),
        }
    }
}

/// One turn of the conversation sent to the model.
///
/// A turn is one of the three shapes the Messages API distinguishes: text, the
/// tool calls the assistant made, or the results answering them. Flattening a
/// tool call to text would not round-trip — the API needs the `tool_use` and
/// `tool_result` blocks paired by id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LlmMessage {
    /// A text turn; `assistant` is true when the model said it.
    Text {
        /// Whether the model, rather than the user, produced this turn.
        assistant: bool,
        /// The turn's text.
        text: String,
    },
    /// The assistant asked to call these tools.
    ToolCalls(Vec<LlmToolCall>),
    /// The results answering the preceding tool calls.
    ToolResults(Vec<LlmToolResult>),
}

/// A tool the model may call, as advertised in the request.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolSpec {
    /// The name the model calls it by.
    pub name: String,
    /// What the tool does, so the model knows when to call it.
    pub description: String,
    /// The JSON Schema of the tool's arguments.
    pub parameters: serde_json::Value,
}

/// A tool call the model asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LlmToolCall {
    /// The provider's id for this call, echoed back with its result.
    pub id: String,
    /// The tool to call.
    pub name: String,
    /// The call's arguments, as the raw JSON string the model produced.
    pub arguments: String,
}

/// The result of one tool call, sent back to the model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LlmToolResult {
    /// The id of the call this answers.
    pub id: String,
    /// The tool's output, or a short message saying why it failed.
    pub content: String,
}

/// One reply from the model.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LlmResponse {
    /// The reply text, before the agent strips a `NO_REPLY`.
    pub text: String,
    /// Input tokens the model reported, when it reported any.
    pub input_tokens: Option<u32>,
    /// Output tokens the model reported, when it reported any.
    pub output_tokens: Option<u32>,
    /// The tools the model asked to call, empty when it answered directly.
    pub tool_calls: Vec<LlmToolCall>,
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
