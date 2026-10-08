//! The milestone-4 exit criteria, end to end.
//!
//! These drive the real [`Agent`] with a scripted model and assert on what the
//! model was *shown* — the prompt the turn built — not on its prose: a fact
//! stated far back in history is retrieved; after a fact changes the newest
//! value is used and the old one is still answerable as history; a private fact
//! never reaches a group. The extraction pass runs first inside `respond`, so a
//! scripted reply list must put the extraction JSON before the turn's reply.

use std::sync::Arc;

use chrono::{Duration, Utc};
use megumi_agent::{
    Agent, AgentConfig, ChatId, ChatType, InboundEvent, LlmResponse, MemoryOp, MemoryStore,
    MessageStore, ScriptedLlm, SenderId, ToolRegistry, TraceSink, Visibility,
};

/// A pipeline over in-memory stores, with memory extraction turned on.
///
/// Returns the agent and the scripted model, so a test can read back the prompts
/// the pipeline built.
fn agent(
    replies: impl IntoIterator<Item = &'static str>,
    extract_batch: usize,
) -> (Agent, Arc<ScriptedLlm>) {
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
    let mut config = AgentConfig::for_test();
    config.memory_extract_batch = extract_batch;
    let agent = Agent::new(
        store,
        memory,
        traces,
        llm.clone(),
        Arc::new(ToolRegistry::new(Vec::new())),
        config,
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
        is_command: false,
        from_self: false,
        timestamp: Utc::now(),
    }
}

fn new_fact(content: &str, evidence: &str) -> megumi_agent::memory::store::NewFact {
    megumi_agent::memory::store::NewFact {
        content: content.into(),
        visibility: Visibility::Chat,
        subject: None,
        confidence: 0.9,
        importance: 0.7,
        valid_from: Utc::now(),
        evidence: vec![evidence.into()],
    }
}

#[tokio::test]
async fn a_fact_far_back_in_history_is_recalled() {
    // The batch is 2, so a pass runs on the second message. The extraction reply
    // comes first; the trigger's reply comes second. The trigger adds nothing
    // new, so no second pass fires.
    let (agent, llm) = agent(
        [
            r#"[{"op":"ADD","content":"The team's venue is the old hall.","evidence":["gA:u1:the venue is the old hall"]}]"#,
            "It is the old hall.",
        ],
        2,
    );

    for text in ["hello everyone", "the venue is the old hall"] {
        let e = event("gA", ChatType::Group, "u1", text);
        agent.ingest(&e).unwrap();
        assert!(agent.respond(&e).await.unwrap().is_none());
    }
    assert_eq!(agent.memory().len().unwrap(), 1, "the fact was extracted");

    let mut trigger = event("gA", ChatType::Group, "u2", "where is the venue?");
    trigger.mentions_self = true;
    agent.ingest(&trigger).unwrap();
    agent.respond(&trigger).await.unwrap();

    // The fact reached the prompt's memories layer.
    let request = llm.requests().pop().unwrap();
    assert!(request.user.contains("<memories>"), "{}", request.user);
    assert!(request.user.contains("the old hall"), "{}", request.user);
}

#[tokio::test]
async fn a_corrected_fact_uses_the_newest_value_and_keeps_the_old_as_history() {
    // Seed the store directly so the ids are known: the exit criterion is about
    // how retrieval renders a corrected fact, not about extraction mechanics.
    let (agent, llm) = agent(["current", "history"], usize::MAX);
    let chat = ChatId::new("gA");
    agent
        .memory()
        .apply(
            &chat,
            &[MemoryOp::Add(new_fact("The venue is the old hall.", "m1"))],
            None,
        )
        .unwrap();
    let old_id = agent.memory().records().unwrap()[0].id.clone();
    let later = Utc::now() + Duration::minutes(1);
    let mut corrected = new_fact("The venue is the new hall.", "m2");
    corrected.valid_from = later;
    agent
        .memory()
        .apply(
            &chat,
            &[MemoryOp::Update {
                target: old_id,
                fact: corrected,
            }],
            None,
        )
        .unwrap();

    // A present-tense question gets the newest value only.
    let mut now = event("gA", ChatType::Group, "u1", "where is the venue?");
    now.mentions_self = true;
    agent.ingest(&now).unwrap();
    agent.respond(&now).await.unwrap();
    let present = llm.requests().pop().unwrap();
    assert!(present.user.contains("new hall"), "{}", present.user);
    assert!(!present.user.contains("old hall"), "{}", present.user);

    // A question about the past gets both, the old one marked as superseded.
    let mut then = event("gA", ChatType::Group, "u1", "where was the venue before?");
    then.mentions_self = true;
    agent.ingest(&then).unwrap();
    agent.respond(&then).await.unwrap();
    let past = llm.requests().pop().unwrap();
    assert!(past.user.contains("new hall"), "{}", past.user);
    assert!(past.user.contains("old hall"), "{}", past.user);
    assert!(past.user.contains("no longer true"), "{}", past.user);
}

#[tokio::test]
async fn a_private_fact_never_reaches_a_group_prompt() {
    // Replies in order: the first DM message is answered ("noted"); the second
    // trips extraction (the canary JSON) and is answered ("ok"); the group turn
    // is answered ("hello group").
    let (agent, llm) = agent(
        [
            "noted",
            r#"[{"op":"ADD","content":"CANARY-private-dm-secret","evidence":["dm:u1:my secret is the canary"]}]"#,
            "ok",
            "hello group",
        ],
        2,
    );

    for text in ["hi", "my secret is the canary"] {
        let e = event("dm", ChatType::Private, "u1", text);
        agent.ingest(&e).unwrap();
        agent.respond(&e).await.unwrap();
    }
    let private = agent.memory().records().unwrap();
    assert_eq!(private.len(), 1);
    assert_eq!(private[0].visibility, Visibility::Private);

    let mut trigger = event("gA", ChatType::Group, "u1", "@bot what is the secret?");
    trigger.mentions_self = true;
    agent.ingest(&trigger).unwrap();
    agent.respond(&trigger).await.unwrap();

    // The group turn's prompt must not carry the private fact, even though the
    // store holds it and the same person is asking.
    let request = llm.requests().pop().unwrap();
    assert!(!request.user.contains("CANARY"), "{}", request.user);
}

#[tokio::test]
async fn a_failed_extraction_pass_does_not_advance_the_cursor() {
    // No scripted reply: the extraction call fails with a transport error, so
    // the cursor must not move and the pass is retried next time.
    let llm = Arc::new(ScriptedLlm::new(Vec::new()));
    let store = Arc::new(MessageStore::open(":memory:", 200).unwrap());
    let memory = Arc::new(MemoryStore::open(":memory:").unwrap());
    let traces = Arc::new(TraceSink::open(":memory:", 500).unwrap());
    let mut config = AgentConfig::for_test();
    config.memory_extract_batch = 2;
    let agent = Agent::new(
        store,
        memory,
        traces,
        llm,
        Arc::new(ToolRegistry::new(Vec::new())),
        config,
    );

    for text in ["one", "two"] {
        let e = event("gA", ChatType::Group, "u1", text);
        agent.ingest(&e).unwrap();
        // The turn fails too (no reply left); the point here is the cursor.
        let _ = agent.respond(&e).await;
    }
    assert!(
        agent.memory().cursor(&ChatId::new("gA")).unwrap().is_none(),
        "a failed pass must not advance the cursor"
    );
}
