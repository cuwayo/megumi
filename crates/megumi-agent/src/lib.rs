//! A conversational agent core, platform-agnostic.
//!
//! This crate is the agent half of Megumi: it turns inbound messages into an
//! LLM-backed reply. It never imports a chat platform's code — a platform
//! adapter converts its own messages into [`InboundEvent`]s and executes the
//! [`OutboundAction`]s that come back. That boundary is what lets the pipeline
//! be tested end-to-end with no WhatsApp connection and no network.
//!
//! The pipeline is: store every message, decide whether to speak
//! ([`gate`]), assemble a budgeted prompt ([`context`]), call the model
//! ([`llm`]), and record what happened ([`trace`]). The pieces are separate
//! modules with typed inputs and outputs so each can be tested and swapped
//! independently.
//!
//! Module map:
//!
//! - `event` - [`InboundEvent`], [`OutboundAction`], and the id newtypes
//! - `config` - [`AgentConfig`], every threshold and budget, env-overridable
//! - `store` - the per-chat message store ([`MessageStore`])
//! - `queues` - [`ChatQueues`], one turn at a time per chat
//! - `gate` - the pure decision of whether a message is answered
//! - `context` - the layered, token-budgeted prompt builder, and the
//!   [`ReaderContext`]/[`Visibility`] privacy boundary
//! - `memory` - durable facts: the store, the extraction writer, the
//!   consolidation/reflection passes, and retrieval
//! - `llm` - the [`LlmClient`] trait, the Anthropic client, and test doubles
//! - `tools` - the [`Tool`] trait, [`ToolContext`], [`ToolRegistry`], and the
//!   built-in memory-search and web-search tools
//! - `safety` - the output guard and the confirmation gate for tools
//! - `reasoning` - the planner and the evaluator that bracket a turn
//! - `trace` - the replayable record of every turn
//! - `agent` - [`Agent`], which runs the pipeline

#![warn(missing_docs)]

pub mod agent;
pub mod config;
pub mod context;
pub mod event;
pub mod gate;
pub mod llm;
pub mod memory;
pub mod queues;
pub mod reasoning;
pub mod safety;
pub mod store;
pub mod tools;
pub mod trace;

/// The crate's error type, the same boxed shape the framework uses.
///
/// The stores and the model return their own concrete errors; `?` lifts each
/// into this one at the pipeline boundary.
pub type Error = Box<dyn std::error::Error + Send + Sync + 'static>;

pub use agent::{Agent, ForgetOutcome};
pub use config::AgentConfig;
pub use context::{ContextBuilder, ReaderContext, Visibility};
pub use event::{
    Attachment, ChatId, ChatType, InboundEvent, OutboundAction, QuotedMessage, SenderId,
};
pub use gate::{GateDecision, Trigger};
pub use llm::{
    AnthropicLlm, DisabledLlm, LlmClient, LlmError, LlmMessage, LlmRequest, LlmResponse,
    LlmToolCall, LlmToolResult, ScriptedLlm, ToolSpec,
};
pub use memory::{MemoryKind, MemoryOp, MemoryRecord, MemoryStore};
pub use queues::ChatQueues;
pub use safety::{Confirmation, PendingConfirmations, PendingToolCall};
pub use store::{MessageStore, StoredMessage};
pub use tools::{SEARCH_MEMORY, SearchMemory, Tool, ToolContext, ToolRegistry, WebSearch};
pub use trace::{ToolCallTrace, TraceSink, TurnTrace};
