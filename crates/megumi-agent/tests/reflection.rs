//! The milestone-9 exit criteria, end to end.
//!
//! These drive the real [`Agent`] with a scripted model and assert on what it
//! stored and what the model was *shown* — a reflection stored from a chat's
//! facts, a fact the model cannot ground dropped, a private reflection that never
//! reaches a group — rather than on the model's prose. No network is involved.
//!
//! The pass runs inside `respond`, after extraction. Extraction is left off
//! (its batch threshold is unreachable under `for_test`), so the only model call
//! a turn makes here is the reflection pass; the facts are seeded directly so
//! their ids are known before the scripted reply is built.

use std::sync::Arc;

use chrono::Utc;
use megumi_agent::memory::store::NewFact;
use megumi_agent::{
    Agent, AgentConfig, ChatId, ChatType, InboundEvent, LlmResponse, MemoryKind, MemoryOp,
    MemoryStore, MessageStore, ScriptedLlm, SenderId, ToolRegistry, TraceSink, Visibility,
};

/// Builds an agent over `memory`, with the reflection pass on and a batch of 2.
///
/// The scripted replies are the model's answers, in call order. Reflection is
/// raised from `for_test`'s off; extraction stays off, so a turn's only model
/// call is the reflection pass.
fn agent(memory: Arc<MemoryStore>, replies: Vec<LlmResponse>) -> (Agent, Arc<ScriptedLlm>) {
    let llm = Arc::new(ScriptedLlm::new(replies));
    let store = Arc::new(MessageStore::open(":memory:", 200).unwrap());
    let traces = Arc::new(TraceSink::open(":memory:", 500).unwrap());
    let mut config = AgentConfig::for_test();
    config.reflection_enabled = true;
    config.reflection_batch = 2;
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
        mentions_self: true,
        is_reply_to_self: false,
        is_command: false,
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

/// Seeds `count` facts into `chat` and returns their ids.
fn seed(memory: &MemoryStore, chat: &str, contents: &[&str]) -> Vec<String> {
    let chat = ChatId::new(chat);
    let ops: Vec<MemoryOp> = contents
        .iter()
        .map(|content| MemoryOp::Add(fact(content, Visibility::Chat)))
        .collect();
    memory.apply(&chat, &ops, None).unwrap();
    memory
        .records()
        .unwrap()
        .into_iter()
        .filter(|record| record.origin_chat == chat)
        .map(|record| record.id)
        .collect()
}

fn text_reply(text: &str) -> LlmResponse {
    LlmResponse {
        text: text.into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn a_chat_with_enough_facts_stores_a_reflection() {
    let memory = Arc::new(MemoryStore::open(":memory:").unwrap());
    let ids = seed(
        &memory,
        "gA",
        &[
            "the team meets on Fridays",
            "the team meets in the old hall",
        ],
    );
    let reply = format!(
        r#"[{{"op":"ADD","content":"The team holds its Friday meeting in the old hall.","confidence":0.8,"importance":0.7,"evidence":["{}","{}"]}}]"#,
        ids[0], ids[1]
    );
    let (agent, llm) = agent(
        Arc::clone(&memory),
        vec![text_reply(&reply), text_reply("hello")],
    );

    let trigger = event("gA", ChatType::Group, "u1", "@bot hi");
    agent.ingest(&trigger).unwrap();
    agent.respond(&trigger).await.unwrap();

    let records = memory.records().unwrap();
    let reflection = records
        .iter()
        .find(|record| record.kind == MemoryKind::Reflection)
        .expect("a reflection was stored");
    assert_eq!(
        reflection.content,
        "The team holds its Friday meeting in the old hall."
    );
    // Its provenance is the facts it was derived from, and its visibility is the
    // chat's, set in code.
    assert_eq!(reflection.source_message_ids, ids);
    assert_eq!(reflection.visibility, Visibility::Chat);
    // The reflection call runs first, and it was asked to consolidate, not
    // answer; the second call is the turn's own reply.
    let requests = llm.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[0].user.contains("<stored_facts>"),
        "{}",
        requests[0].user
    );
}

#[tokio::test]
async fn a_pass_citing_an_unknown_fact_stores_nothing() {
    let memory = Arc::new(MemoryStore::open(":memory:").unwrap());
    seed(&memory, "gA", &["one fact", "another fact"]);
    let reply = r#"[{"op":"ADD","content":"An ungrounded insight.","evidence":["ghost"]}]"#;
    let (agent, _llm) = agent(Arc::clone(&memory), vec![text_reply(reply)]);

    let trigger = event("gA", ChatType::Group, "u1", "@bot hi");
    agent.ingest(&trigger).unwrap();
    agent.respond(&trigger).await.unwrap();

    assert!(
        memory
            .records()
            .unwrap()
            .iter()
            .all(|record| record.kind == MemoryKind::Fact),
        "an op citing a fact that does not exist must be dropped"
    );
}

#[tokio::test]
async fn a_private_reflection_never_reaches_a_group_prompt() {
    let memory = Arc::new(MemoryStore::open(":memory:").unwrap());
    let dm_ids = seed(
        &memory,
        "dm",
        &[
            "the owner's secret plan is the canary",
            "the owner plans a trip",
        ],
    );
    let reply = format!(
        r#"[{{"op":"ADD","content":"CANARY-private-reflection-secret","evidence":["{}"]}}]"#,
        dm_ids[0]
    );
    let (agent, llm) = agent(
        Arc::clone(&memory),
        vec![text_reply(&reply), text_reply("hello")],
    );

    // A private turn forms the private reflection.
    let dm = event("dm", ChatType::Private, "u1", "what's up?");
    agent.ingest(&dm).unwrap();
    agent.respond(&dm).await.unwrap();
    assert!(
        memory
            .records()
            .unwrap()
            .iter()
            .any(|record| record.kind == MemoryKind::Reflection)
    );

    // A group turn must not see it, even for the same person asking.
    let mut group = event("gA", ChatType::Group, "u1", "@bot what do you know?");
    group.mentions_self = true;
    agent.ingest(&group).unwrap();
    agent.respond(&group).await.unwrap();

    let request = llm.requests().pop().unwrap();
    assert!(!request.user.contains("CANARY"), "{}", request.user);
}

#[tokio::test]
async fn a_duplicate_fact_the_extraction_proposes_is_not_stored_twice() {
    // Consolidation also guards the extraction path: when the writer proposes a
    // fact the chat already holds, it is dropped before it reaches the store.
    let memory = Arc::new(MemoryStore::open(":memory:").unwrap());
    // Seed one live fact with the exact wording the extraction will propose.
    seed(&memory, "gA", &["the venue is the old hall"]);

    // Extraction on (batch of 1) and reflection off, so the only model call is
    // the extraction pass — whose ADD repeats the stored fact.
    let trigger = event("gA", ChatType::Group, "u1", "@bot hello");
    let reply = format!(
        r#"[{{"op":"ADD","content":"The venue is the old hall.","evidence":["{}"]}}]"#,
        trigger.message_id
    );
    let llm = Arc::new(ScriptedLlm::new(vec![
        text_reply(&reply),
        text_reply("noted"),
    ]));
    let store = Arc::new(MessageStore::open(":memory:", 200).unwrap());
    let traces = Arc::new(TraceSink::open(":memory:", 500).unwrap());
    let mut config = AgentConfig::for_test();
    config.memory_extract_batch = 1;
    let agent = Agent::new(
        store,
        Arc::clone(&memory),
        traces,
        llm,
        Arc::new(ToolRegistry::new(Vec::new())),
        config,
    );

    agent.ingest(&trigger).unwrap();
    agent.respond(&trigger).await.unwrap();

    // The duplicate was dropped: the one seeded fact is still the only one.
    assert_eq!(memory.len().unwrap(), 1);
}

#[tokio::test]
async fn reflections_off_makes_no_call() {
    // The default `for_test` config has reflection off; a chat with plenty of
    // facts must not spend a model call on one.
    let memory = Arc::new(MemoryStore::open(":memory:").unwrap());
    seed(&memory, "gA", &["one", "two", "three"]);
    let llm = Arc::new(ScriptedLlm::new(vec![text_reply("hello")]));
    let store = Arc::new(MessageStore::open(":memory:", 200).unwrap());
    let traces = Arc::new(TraceSink::open(":memory:", 500).unwrap());
    let agent = Agent::new(
        store,
        Arc::clone(&memory),
        traces,
        llm.clone(),
        Arc::new(ToolRegistry::new(Vec::new())),
        AgentConfig::for_test(),
    );

    let trigger = event("gA", ChatType::Group, "u1", "@bot hi");
    agent.ingest(&trigger).unwrap();
    agent.respond(&trigger).await.unwrap();

    assert_eq!(llm.requests().len(), 1, "only the turn's own call");
    assert!(
        memory
            .records()
            .unwrap()
            .iter()
            .all(|record| record.kind == MemoryKind::Fact)
    );
}

#[tokio::test]
async fn a_noop_reflection_does_not_spin_on_the_next_turn() {
    let memory = Arc::new(MemoryStore::open(":memory:").unwrap());
    seed(&memory, "gA", &["one fact", "another fact"]);
    // First turn: the reflection pass runs and returns NOOP. Second turn: no new
    // facts, so the cursor stops it and only the turn's own reply is used.
    let (agent, llm) = agent(
        Arc::clone(&memory),
        vec![text_reply(r#"[{"op":"NOOP"}]"#), text_reply("hello again")],
    );

    for text in ["@bot hi", "@bot hello again"] {
        let trigger = event("gA", ChatType::Group, "u1", text);
        agent.ingest(&trigger).unwrap();
        agent.respond(&trigger).await.unwrap();
    }

    // One reflection call and two turn calls — the NOOP pass did not re-fire.
    assert_eq!(llm.requests().len(), 3);
    assert!(
        memory
            .reflection_cursor(&ChatId::new("gA"))
            .unwrap()
            .is_some()
    );
}
