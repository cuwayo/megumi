//! The milestone-7 exit criteria: the command router's agent seams.
//!
//! These drive the real [`Agent`] the way the `!ask`, `!summary`, `!memory`, and
//! `!forget` commands do — with a scripted model and no network — and assert on
//! what the model was shown and what the command-facing methods returned. They
//! cover the behaviours the design treats as exit criteria: a command forces a
//! turn the gate would otherwise refuse, a summary reaches the next prompt's
//! summary layer, and memory listing and forgetting respect the chat and the
//! reader's privacy boundary.

use std::sync::Arc;

use chrono::Utc;
use megumi_agent::memory::store::NewFact;
use megumi_agent::{
    Agent, AgentConfig, ChatId, ChatType, ForgetOutcome, InboundEvent, LlmResponse, MemoryOp,
    MemoryStore, MessageStore, OutboundAction, ScriptedLlm, SenderId, ToolRegistry, TraceSink,
    Visibility,
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
        message_id: format!("{chat}:{sender}:{text}"),
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
        is_command: true,
        from_self: false,
        timestamp: Utc::now(),
    }
}

fn fact(content: &str, visibility: Visibility) -> NewFact {
    NewFact {
        content: content.into(),
        visibility,
        subject: None,
        confidence: 0.9,
        importance: 0.6,
        valid_from: Utc::now(),
        evidence: vec!["m1".into()],
    }
}

#[tokio::test]
async fn a_command_forces_a_turn_the_gate_would_refuse() {
    let (agent, _llm) = agent(["Paris"]);
    let ask = event(
        "gA",
        ChatType::Group,
        "u1",
        "what is the capital of France?",
    );
    agent.ingest(&ask).unwrap();

    // The gate alone would stay silent on a command.
    assert!(agent.respond(&ask).await.unwrap().is_none());

    let action = agent.answer_command(&ask).await.unwrap();
    assert_eq!(
        action,
        Some(OutboundAction::SendText {
            chat: ChatId::new("gA"),
            text: "Paris".into(),
        })
    );
    let traces = agent.traces().recent(10).unwrap();
    assert_eq!(traces[0].trigger, "command");
}

#[tokio::test]
async fn a_stored_question_reaches_the_prompts_history() {
    // The command stores its own message before forcing the turn, so the model
    // sees the question even though the raw message is a command.
    let (agent, llm) = agent(["Paris"]);
    let ask = event(
        "gA",
        ChatType::Group,
        "u1",
        "what is the capital of France?",
    );
    agent.ingest(&ask).unwrap();
    agent.answer_command(&ask).await.unwrap();

    let requests = llm.requests();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].user.contains("capital of France"),
        "{}",
        requests[0].user
    );
}

#[tokio::test]
async fn a_summary_reaches_the_next_turns_summary_layer() {
    // The summary reply comes first; the later turn's reply second.
    let (agent, llm) = agent(["They discussed the venue.", "sure"]);
    agent
        .ingest(&event(
            "gA",
            ChatType::Group,
            "u1",
            "the venue is the old hall",
        ))
        .unwrap();

    let summary = agent
        .summarize(&ChatId::new("gA"), ChatType::Group)
        .await
        .unwrap();
    assert_eq!(summary.as_deref(), Some("They discussed the venue."));

    // A later turn's prompt carries the stored summary.
    let mut trigger = event("gA", ChatType::Group, "u1", "what next?");
    trigger.is_command = false;
    trigger.mentions_self = true;
    agent.ingest(&trigger).unwrap();
    agent.respond(&trigger).await.unwrap();

    let requests = llm.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1].user.contains("They discussed the venue."),
        "{}",
        requests[1].user
    );
}

#[tokio::test]
async fn listing_memory_is_scoped_to_the_chat_and_the_reader() {
    let (agent, _llm) = agent(["unused"]);
    agent
        .memory()
        .apply(
            &ChatId::new("gA"),
            &[MemoryOp::Add(fact(
                "the venue is the old hall",
                Visibility::Chat,
            ))],
            None,
        )
        .unwrap();
    agent
        .memory()
        .apply(
            &ChatId::new("dm"),
            &[MemoryOp::Add(fact(
                "Budi lives in Bandung",
                Visibility::Private,
            ))],
            None,
        )
        .unwrap();

    let group = event("gA", ChatType::Group, "u1", "!memory");
    let listed = agent.list_memory(&group).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].content, "the venue is the old hall");

    // A different group sees none of gA's facts.
    let other = event("gB", ChatType::Group, "u1", "!memory");
    assert!(agent.list_memory(&other).await.unwrap().is_empty());
}

#[tokio::test]
async fn forgetting_makes_a_fact_unreachable_even_for_a_past_question() {
    let (agent, _llm) = agent(["unused"]);
    let chat = ChatId::new("gA");
    agent
        .memory()
        .apply(
            &chat,
            &[MemoryOp::Add(fact(
                "the venue is the old hall",
                Visibility::Chat,
            ))],
            None,
        )
        .unwrap();
    let id = agent.memory().records().unwrap()[0].id.clone();

    let forget = event("gA", ChatType::Group, "u1", "!forget");
    assert_eq!(
        agent.forget_memory(&forget, &id).await.unwrap(),
        ForgetOutcome::Forgotten { count: 1 }
    );

    // Even a question about the past no longer recalls it.
    let ask = event("gA", ChatType::Group, "u1", "where was the venue before?");
    assert!(agent.list_memory(&ask).await.unwrap().is_empty());
}

#[tokio::test]
async fn forget_all_clears_only_this_chats_facts() {
    let (agent, _llm) = agent(["unused"]);
    agent
        .memory()
        .apply(
            &ChatId::new("gA"),
            &[
                MemoryOp::Add(fact("one", Visibility::Chat)),
                MemoryOp::Add(fact("two", Visibility::Chat)),
            ],
            None,
        )
        .unwrap();
    agent
        .memory()
        .apply(
            &ChatId::new("gB"),
            &[MemoryOp::Add(fact("other group", Visibility::Chat))],
            None,
        )
        .unwrap();

    let forget = event("gA", ChatType::Group, "u1", "!forget all");
    assert_eq!(
        agent.forget_memory(&forget, "all").await.unwrap(),
        ForgetOutcome::Forgotten { count: 2 }
    );

    assert!(agent.list_memory(&forget).await.unwrap().is_empty());
    // The other group's fact is untouched.
    let other = event("gB", ChatType::Group, "u1", "!memory");
    assert_eq!(agent.list_memory(&other).await.unwrap().len(), 1);
}
