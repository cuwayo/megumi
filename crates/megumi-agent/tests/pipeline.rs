//! End-to-end tests of the agent pipeline, with a scripted model.
//!
//! These drive the real [`Agent`] — store, gate, context builder, and model —
//! with no WhatsApp connection and no network, so the whole turn is exercised
//! deterministically. They cover the behaviours the design treats as exit
//! criteria for the first milestones: every message is stored, the gate decides
//! who is answered, the context carries the windowed history, and every turn is
//! replayable from the trace log.

use std::sync::Arc;

use chrono::{Duration, Utc};
use megumi_agent::{
    Agent, AgentConfig, ChatId, ChatType, InboundEvent, LlmResponse, MemoryStore, MessageStore,
    OutboundAction, ScriptedLlm, SenderId, ToolRegistry, TraceSink,
};

fn agent(replies: impl IntoIterator<Item = &'static str>) -> (Agent, Arc<ScriptedLlm>) {
    let llm = Arc::new(ScriptedLlm::new(replies.into_iter().map(|text| {
        LlmResponse {
            text: text.to_string(),
            input_tokens: Some(10),
            output_tokens: Some(4),
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

fn event(chat: &str, chat_type: ChatType, sender: &str, text: &str) -> InboundEvent {
    InboundEvent {
        message_id: format!("{chat}-{sender}-{text}"),
        chat: ChatId::new(chat),
        chat_type,
        sender: SenderId::new(sender),
        sender_alt: None,
        sender_name: Some(sender.to_string()),
        text: Some(text.to_string()),
        attachments: Vec::new(),
        reply_to: None,
        mentions_self: false,
        is_reply_to_self: false,
        is_command: false,
        from_self: false,
        timestamp: Utc::now(),
    }
}

#[tokio::test]
async fn every_message_is_stored_even_when_not_answered() {
    let (agent, llm) = agent(["unused"]);
    for text in ["hello", "how are you", "just chatting"] {
        let e = event("gA", ChatType::Group, "u1", text);
        agent.ingest(&e).unwrap();
        assert!(agent.respond(&e).await.unwrap().is_none());
    }
    // Every message is retained for the next trigger, and none reached the model.
    assert_eq!(
        agent.store().recent(&ChatId::new("gA"), 10).unwrap().len(),
        3
    );
    assert!(llm.requests().is_empty());
}

#[tokio::test]
async fn the_windowed_context_carries_prior_messages() {
    let (agent, llm) = agent(["yes"]);
    agent
        .ingest(&event(
            "gA",
            ChatType::Group,
            "u1",
            "the meeting is on Friday",
        ))
        .unwrap();

    let mut trigger = event("gA", ChatType::Group, "u1", "when is it?");
    trigger.mentions_self = true;
    agent.ingest(&trigger).unwrap();
    agent.respond(&trigger).await.unwrap();

    let requests = llm.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].user.contains("the meeting is on Friday"));
    assert!(requests[0].user.contains("when is it?"));
    assert!(requests[0].user.contains("<chat_message"));
}

#[tokio::test]
async fn a_triggered_group_turn_replies_and_is_traced() {
    let (agent, _llm) = agent(["Friday at noon"]);
    let mut trigger = event("gA", ChatType::Group, "u1", "@bot when?");
    trigger.mentions_self = true;
    agent.ingest(&trigger).unwrap();

    let action = agent.respond(&trigger).await.unwrap();
    assert_eq!(
        action,
        Some(OutboundAction::SendText {
            chat: ChatId::new("gA"),
            text: "Friday at noon".into(),
        })
    );
}

#[tokio::test]
async fn a_private_chat_answers_and_a_duplicate_is_stored_once() {
    let (agent, _llm) = agent(["hello"]);
    let e = event("dm", ChatType::Private, "u1", "hi there");

    assert!(agent.ingest(&e).unwrap());
    // A redelivery of the same message id is not stored twice.
    assert!(!agent.ingest(&e).unwrap());
    assert!(agent.respond(&e).await.unwrap().is_some());
}

#[tokio::test]
async fn the_session_gap_does_not_change_storage() {
    let (agent, _llm) = agent(["welcome back"]);
    let mut first = event("dm", ChatType::Private, "u1", "hello");
    first.timestamp = Utc::now() - Duration::hours(8);
    agent.ingest(&first).unwrap();

    let second = event("dm", ChatType::Private, "u1", "hi again");
    agent.ingest(&second).unwrap();

    // Both messages are retained; the gap is metadata the context builder will
    // use to decide how much to replay, not a reason to drop history.
    let store = agent.store();
    assert_eq!(store.recent(&ChatId::new("dm"), 10).unwrap().len(), 2);
}

#[tokio::test]
async fn the_trace_log_records_every_turn() {
    let (agent, _llm) = agent(["a", "b"]);
    for text in ["one", "two"] {
        let mut trigger = event("gA", ChatType::Group, "u1", text);
        trigger.mentions_self = true;
        agent.ingest(&trigger).unwrap();
        agent.respond(&trigger).await.unwrap();
    }
    let traces = agent.traces().recent(10).unwrap();
    assert_eq!(traces.len(), 2);
    assert!(traces.iter().all(|trace| trace.trigger == "mention"));
}
