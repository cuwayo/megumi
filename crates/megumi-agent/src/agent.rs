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
use crate::context::{ContextBuilder, ReaderContext};
use crate::event::{ChatType, InboundEvent, OutboundAction};
use crate::gate::{self, GateDecision, Trigger};
use crate::llm::{LlmClient, LlmError, LlmRequest};
use crate::memory::{MemoryStore, retrieval, writer};
use crate::queues::ChatQueues;
use crate::store::{MessageStore, StoredMessage};
use crate::trace::{self, TraceSink, TurnTrace};

/// The literal reply a model uses to decline to speak.
const NO_REPLY: &str = "NO_REPLY";

/// The agent: the store it reads and writes, the model it asks, and the queues
/// that keep a chat's turns in order.
pub struct Agent {
    store: Arc<MessageStore>,
    memory: Arc<MemoryStore>,
    traces: Arc<TraceSink>,
    llm: Arc<dyn LlmClient>,
    config: AgentConfig,
    queues: ChatQueues,
}

impl Agent {
    /// Builds an agent over an existing store, memory, trace log, and model.
    pub fn new(
        store: Arc<MessageStore>,
        memory: Arc<MemoryStore>,
        traces: Arc<TraceSink>,
        llm: Arc<dyn LlmClient>,
        config: AgentConfig,
    ) -> Self {
        Self {
            store,
            memory,
            traces,
            llm,
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

        let request =
            LlmRequest::from_prompt(&prompt, &self.config.model, self.config.max_reply_tokens);
        let started = Instant::now();
        let response = self.llm.complete(request).await;
        let latency = started.elapsed();

        let (reply, input_tokens, output_tokens) = match &response {
            Ok(response) => (
                Some(response.text.clone()),
                response.input_tokens,
                response.output_tokens,
            ),
            Err(LlmError::Disabled) => {
                debug!(chat = %event.chat, "no model is configured; not replying");
                (None, None, None)
            }
            Err(error) => {
                warn!(chat = %event.chat, %error, "the agent turn failed");
                (None, None, None)
            }
        };

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
            reply: final_reply.clone(),
            timestamp: chrono::Utc::now(),
        })?;

        Ok(final_reply.map(|text| OutboundAction::SendText {
            chat: event.chat.clone(),
            text,
        }))
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
            }
        })));
        let store = Arc::new(MessageStore::open(":memory:", 200).unwrap());
        let memory = Arc::new(MemoryStore::open(":memory:").unwrap());
        let traces = Arc::new(TraceSink::open(":memory:", 500).unwrap());
        let agent = Agent::new(store, memory, traces, llm.clone(), AgentConfig::for_test());
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
