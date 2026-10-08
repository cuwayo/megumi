//! The turn pipeline: store, decide, build context, ask the model, record.
//!
//! [`Agent::ingest`] stores one message; [`Agent::respond`] runs a turn for a
//! message and returns what to send, if anything. They are separate on purpose:
//! storing must never wait on — or fail because of — a model call, so the
//! adapter stores every message first and only then runs the turns that a
//! trigger calls for. A turn is serialized per chat through [`ChatQueues`].

use std::sync::Arc;
use std::time::Instant;

use tracing::{debug, warn};

use crate::config::AgentConfig;
use crate::context::{ContextBuilder, ReaderContext, render_memory};
use crate::event::{ChatType, InboundEvent, OutboundAction};
use crate::gate::{self, GateDecision, Trigger};
use crate::llm::{
    LlmClient, LlmError, LlmMessage, LlmRequest, LlmToolCall, LlmToolResult, ToolSpec,
};
use crate::memory::{MemoryStore, retrieval, writer};
use crate::queues::ChatQueues;
use crate::store::{MessageStore, StoredMessage};
use crate::tools::ToolRegistry;
use crate::trace::{self, ToolCallTrace, TraceSink, TurnTrace};

/// The literal reply a model uses to decline to speak.
const NO_REPLY: &str = "NO_REPLY";

/// The name of the per-turn memory-search tool.
const SEARCH_MEMORY: &str = "search_memory";

/// The agent: the store it reads and writes, the model it asks, and the queues
/// that keep a chat's turns in order.
pub struct Agent {
    store: Arc<MessageStore>,
    memory: Arc<MemoryStore>,
    traces: Arc<TraceSink>,
    llm: Arc<dyn LlmClient>,
    tools: Arc<ToolRegistry>,
    config: AgentConfig,
    queues: ChatQueues,
}

impl Agent {
    /// Builds an agent over an existing store, memory, trace log, model, and
    /// tool registry.
    pub fn new(
        store: Arc<MessageStore>,
        memory: Arc<MemoryStore>,
        traces: Arc<TraceSink>,
        llm: Arc<dyn LlmClient>,
        tools: Arc<ToolRegistry>,
        config: AgentConfig,
    ) -> Self {
        Self {
            store,
            memory,
            traces,
            llm,
            tools,
            config,
            queues: ChatQueues::new(),
        }
    }

    /// Stores one message for `event`'s chat.
    ///
    /// Returns whether the message was new. Every message is stored, in every
    /// chat, whether or not the agent answers — that history is what context
    /// and, later, memory are built from. A duplicate id is a no-op, so
    /// redelivery never stores twice.
    pub fn ingest(&self, event: &InboundEvent) -> Result<bool, crate::Error> {
        let message = StoredMessage {
            message_id: event.message_id.clone(),
            sender: event.sender.clone(),
            sender_name: event.sender_name.clone(),
            text: event.text.clone(),
            from_self: event.from_self,
            timestamp: event.timestamp,
        };
        Ok(self.store.append(&event.chat, message)?)
    }

    /// Runs a turn for `event`, serialized per chat.
    ///
    /// Returns the action to take, or `None` when the message must not be
    /// answered — either because the gate says so, or because the model chose
    /// `NO_REPLY`. A duplicate of a message already answered is not answered
    /// again: the adapter stores messages before responding, so a redelivered
    /// trigger is already present when it arrives here.
    pub async fn respond(
        &self,
        event: &InboundEvent,
    ) -> Result<Option<OutboundAction>, crate::Error> {
        let _guard = self.queues.lock(&event.chat).await;

        // Extract before deciding whether to answer: the adapter calls this for
        // every message, so a busy group that never triggers the agent still
        // turns its messages into facts before the message window prunes them.
        // A failed pass is logged, never fatal to the turn — memory is a
        // background concern, the reply is not.
        if let Err(error) = writer::extract_if_due(
            self.llm.as_ref(),
            &self.store,
            &self.memory,
            &self.config,
            &event.chat,
            event.chat_type,
        )
        .await
        {
            warn!(chat = %event.chat, %error, "the memory extraction pass failed");
        }

        match gate::decide(event) {
            GateDecision::StaySilent(reason) => {
                debug!(chat = %event.chat, ?reason, "the agent stayed silent");
                Ok(None)
            }
            GateDecision::Respond(trigger) => self.run_turn(event, trigger).await,
        }
    }

    async fn run_turn(
        &self,
        event: &InboundEvent,
        trigger: Trigger,
    ) -> Result<Option<OutboundAction>, crate::Error> {
        let reader = self.reader_for(event);
        let window = match event.chat_type {
            ChatType::Group => self.config.group_window,
            ChatType::Private => self.config.private_window,
        };
        let history = self.store.recent(&event.chat, window)?;
        let summary = self.store.summary(&event.chat)?;

        // The facts this reader may see, filtered and ranked by the retrieval
        // layer's privacy boundary. The context builder re-checks every one.
        let memories = retrieval::search(
            &reader,
            event.trimmed_text(),
            &self.memory,
            &self.config,
            chrono::Utc::now(),
        )?;
        let prompt = ContextBuilder::new(&self.config).build(
            &reader,
            summary.as_deref(),
            &memories,
            &history,
            event,
        );

        // The tools the model may call this turn: the reader-bound memory
        // search, then the registry's reader-independent tools.
        let tools = self.turn_tools();

        let started = Instant::now();
        let mut messages = vec![LlmMessage::Text {
            assistant: false,
            text: prompt.user.clone(),
        }];
        let mut calls: Vec<ToolCallTrace> = Vec::new();
        // Summed over the turn's calls, staying `None` until a call reports a
        // count — the same answer a single call gave before the loop existed.
        let mut input_tokens: Option<u32> = None;
        let mut output_tokens: Option<u32> = None;

        // The loop is bounded: the model normally answers in one call, and a
        // model that keeps calling tools is cut off rather than looped forever.
        // The last iteration is a final chance to answer, so it advertises no
        // tools — a call there would have no turn left to use its result.
        let mut reply = None;
        let max_iterations = self.config.max_tool_iterations;
        for iteration in 0..=max_iterations {
            let last = iteration == max_iterations;
            let request = LlmRequest {
                model: self.config.model.clone(),
                system: prompt.system.clone(),
                user: prompt.user.clone(),
                max_tokens: self.config.max_reply_tokens,
                tools: if last { Vec::new() } else { tools.clone() },
                messages: messages.clone(),
            };
            let response = match self.llm.complete(request).await {
                Ok(response) => response,
                Err(LlmError::Disabled) => {
                    debug!(chat = %event.chat, "no model is configured; not replying");
                    break;
                }
                Err(error) => {
                    warn!(chat = %event.chat, %error, "the agent turn failed");
                    break;
                }
            };
            input_tokens = add_tokens(input_tokens, response.input_tokens);
            output_tokens = add_tokens(output_tokens, response.output_tokens);

            // The last iteration answers with what it has; there is no turn left
            // to use a tool result, so any call it still makes is not run.
            if last || response.tool_calls.is_empty() {
                reply = Some(response.text);
                break;
            }

            // The model asked for tools. Run each, record it, and feed the
            // results back as the next turn's input.
            let results = self
                .run_tool_calls(&reader, &response.tool_calls, &mut calls)
                .await;
            messages.push(LlmMessage::ToolCalls(response.tool_calls));
            messages.push(LlmMessage::ToolResults(results));
        }
        let latency = started.elapsed();

        let final_reply = reply.filter(|text| {
            // `NO_REPLY` is the model declining to speak; an empty reply says the
            // same thing.
            let trimmed = text.trim();
            !trimmed.is_empty() && !trimmed.eq_ignore_ascii_case(NO_REPLY)
        });
        self.traces.record(TurnTrace {
            turn_id: uuid::Uuid::new_v4().to_string(),
            chat: event.chat.clone(),
            trigger: TurnTrace::trigger_label(trigger).to_string(),
            model: self.config.model.clone(),
            context_hash: trace::context_hash(&prompt.system, &prompt.user),
            system_chars: prompt.system.chars().count(),
            user_chars: prompt.user.chars().count(),
            input_tokens,
            output_tokens,
            latency_ms: u64::try_from(latency.as_millis()).unwrap_or(u64::MAX),
            tool_calls: calls,
            reply: final_reply.clone(),
            timestamp: chrono::Utc::now(),
        })?;

        Ok(final_reply.map(|text| OutboundAction::SendText {
            chat: event.chat.clone(),
            text,
        }))
    }

    /// The tools a turn may call: the reader-bound memory search first, then
    /// the registry's.
    fn turn_tools(&self) -> Vec<ToolSpec> {
        let mut tools = Vec::new();
        if self.config.max_tool_iterations > 0 {
            tools.push(memory_tool_spec());
            tools.extend(self.tools.specs());
        }
        tools
    }

    /// Runs every tool call `calls` asked for, recording each and returning the
    /// results to hand back to the model.
    ///
    /// A tool that fails returns a short error string as its result rather than
    /// failing the turn, so the model can still answer around it.
    async fn run_tool_calls(
        &self,
        reader: &ReaderContext,
        calls: &[LlmToolCall],
        recorded: &mut Vec<ToolCallTrace>,
    ) -> Vec<LlmToolResult> {
        let mut results = Vec::with_capacity(calls.len());
        for call in calls {
            let arguments: serde_json::Value =
                serde_json::from_str(&call.arguments).unwrap_or(serde_json::json!({}));
            let result = if call.name == SEARCH_MEMORY {
                self.search_memory(reader, &arguments)
            } else if let Some(tool) = self.tools.get(&call.name) {
                tool.call(&arguments).await
            } else {
                Err(format!("no tool named `{}`", call.name))
            };
            let content = match result {
                Ok(content) => content,
                Err(error) => format!("the tool failed: {error}"),
            };
            recorded.push(ToolCallTrace {
                name: call.name.clone(),
                arguments: call.arguments.clone(),
                result: content.clone(),
            });
            results.push(LlmToolResult {
                id: call.id.clone(),
                content,
            });
        }
        results
    }

    /// The `search_memory` tool: the facts `reader` may see for a query.
    fn search_memory(
        &self,
        reader: &ReaderContext,
        arguments: &serde_json::Value,
    ) -> Result<String, String> {
        let query = arguments
            .get("query")
            .and_then(|query| query.as_str())
            .ok_or_else(|| "search_memory needs a `query` argument".to_string())?;
        let memories = retrieval::search(
            reader,
            query,
            &self.memory,
            &self.config,
            chrono::Utc::now(),
        )?;
        if memories.is_empty() {
            return Ok("No stored facts matched.".to_string());
        }
        Ok(memories
            .iter()
            .map(render_memory)
            .collect::<Vec<_>>()
            .join("\n"))
    }

    /// The message store, for tests and diagnostics.
    pub fn store(&self) -> &MessageStore {
        &self.store
    }

    /// The memory store, for tests and diagnostics.
    pub fn memory(&self) -> &MemoryStore {
        &self.memory
    }

    /// The trace log, for tests and diagnostics.
    pub fn traces(&self) -> &TraceSink {
        &self.traces
    }

    /// The privacy context for a turn triggered by `event`.
    ///
    /// A group turn's reader is a member of that group only. A private turn's
    /// reader is the person, and the groups they are known to be in are not yet
    /// threaded through — the adapter will supply them when it can, so for now a
    /// private reader sees their private memories and nothing group-scoped.
    fn reader_for(&self, event: &InboundEvent) -> ReaderContext {
        let member_of = match event.chat_type {
            ChatType::Group => vec![event.chat.clone()],
            ChatType::Private => Vec::new(),
        };
        ReaderContext {
            chat: event.chat.clone(),
            chat_type: event.chat_type,
            requester: event.sender.clone(),
            member_of,
        }
    }
}

/// Adds a call's token count to the turn's running total.
///
/// A count stays `None` until some call reports one, so a provider that reports
/// no usage yields `None` rather than a misleading `0`.
fn add_tokens(total: Option<u32>, reported: Option<u32>) -> Option<u32> {
    match (total, reported) {
        (Some(total), Some(reported)) => Some(total + reported),
        (Some(total), None) => Some(total),
        (None, reported) => reported,
    }
}

/// The spec of the per-turn `search_memory` tool.
///
/// The tool is not in the registry because its result depends on the turn's
/// reader, which the registry does not have; the spec itself is reader-free.
fn memory_tool_spec() -> ToolSpec {
    ToolSpec {
        name: SEARCH_MEMORY.to_string(),
        description: "Search the facts this conversation has stored about people, places, \
                      plans, and preferences. Use it when the answer may depend on something \
                      said earlier that is no longer in the recent messages."
            .to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "What to look up in memory."
                }
            },
            "required": ["query"]
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{ChatId, SenderId};
    use crate::llm::ScriptedLlm;
    use chrono::Utc;

    fn agent(replies: impl IntoIterator<Item = &'static str>) -> (Agent, Arc<ScriptedLlm>) {
        let llm = Arc::new(ScriptedLlm::new(replies.into_iter().map(|text| {
            crate::llm::LlmResponse {
                text: text.to_string(),
                input_tokens: Some(1),
                output_tokens: Some(1),
                ..Default::default()
            }
        })));
        let store = Arc::new(MessageStore::open(":memory:", 200).unwrap());
        let memory = Arc::new(MemoryStore::open(":memory:").unwrap());
        let traces = Arc::new(TraceSink::open(":memory:", 500).unwrap());
        let agent = Agent::new(
            store,
            memory,
            traces,
            llm.clone(),
            Arc::new(ToolRegistry::new(Vec::new())),
            AgentConfig::for_test(),
        );
        (agent, llm)
    }

    fn group_event(text: &str, mentioned: bool) -> InboundEvent {
        InboundEvent {
            message_id: format!("m-{text}"),
            chat: ChatId::new("gA"),
            chat_type: ChatType::Group,
            sender: SenderId::new("u1"),
            sender_alt: None,
            sender_name: Some("Budi".into()),
            text: Some(text.into()),
            attachments: Vec::new(),
            reply_to: None,
            mentions_self: mentioned,
            is_reply_to_self: false,
            is_command: false,
            from_self: false,
            timestamp: Utc::now(),
        }
    }

    #[tokio::test]
    async fn an_untriggered_group_message_is_stored_but_not_answered() {
        let (agent, llm) = agent(["should not be used"]);
        let event = group_event("just chatting", false);
        assert!(agent.ingest(&event).unwrap());
        assert!(agent.respond(&event).await.unwrap().is_none());
        assert!(llm.requests().is_empty());
        assert_eq!(agent.store.len(&event.chat).unwrap(), 1);
    }

    #[tokio::test]
    async fn a_mentioned_group_message_is_answered() {
        let (agent, _llm) = agent(["hello there"]);
        let event = group_event("hi bot", true);
        agent.ingest(&event).unwrap();
        let action = agent.respond(&event).await.unwrap();
        assert_eq!(
            action,
            Some(OutboundAction::SendText {
                chat: ChatId::new("gA"),
                text: "hello there".into(),
            })
        );
    }

    #[tokio::test]
    async fn no_reply_from_the_model_means_silence() {
        let (agent, _llm) = agent(["NO_REPLY"]);
        let event = group_event("hi bot", true);
        agent.ingest(&event).unwrap();
        assert!(agent.respond(&event).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_turn_is_recorded_in_the_trace_log() {
        let (agent, _llm) = agent(["hello"]);
        let event = group_event("hi bot", true);
        agent.ingest(&event).unwrap();
        agent.respond(&event).await.unwrap();
        let traces = agent.traces.recent(10).unwrap();
        assert_eq!(traces.len(), 1);
        assert_eq!(traces[0].trigger, "mention");
        assert_eq!(traces[0].reply.as_deref(), Some("hello"));
        assert_eq!(traces[0].input_tokens, Some(1));
    }

    #[tokio::test]
    async fn a_private_message_is_answered() {
        let (agent, _llm) = agent(["of course"]);
        let mut event = group_event("what's up?", false);
        event.chat = ChatId::new("dm");
        event.chat_type = ChatType::Private;
        agent.ingest(&event).unwrap();
        let action = agent.respond(&event).await.unwrap();
        assert!(matches!(action, Some(OutboundAction::SendText { .. })));
    }

    #[test]
    fn a_reply_is_detected_from_the_history_window() {
        let (agent, _llm) = agent(["hello"]);
        let event = group_event("hi bot", true);
        agent.ingest(&event).unwrap();
        let history = agent.store.recent(&event.chat, 10).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].sender_name.as_deref(), Some("Budi"));
    }
}
