//! The milestone-13 exit criteria: the history-search tool.
//!
//! These drive the real [`Agent`] with a scripted model and assert on what the
//! model was *shown* — the prompt it received and the tool result the loop
//! produced. The point of the tool is that a message older than the prompt's
//! window is unreachable directly but reachable by asking, so the tests store
//! more messages than the window carries and prove the difference.

use std::sync::Arc;

use megumi_agent::{
    Agent, AgentConfig, ChatId, ChatType, InboundEvent, LlmMessage, LlmResponse, LlmToolCall,
    MessageStore, ScriptedLlm, SearchHistory, SenderId, Tool, ToolRegistry, TraceSink,
};

/// An agent whose prompt window is `group_window` messages and which advertises
/// `search_history` over its own store.
fn agent(replies: Vec<LlmResponse>, group_window: usize) -> (Agent, Arc<ScriptedLlm>) {
    let llm = Arc::new(ScriptedLlm::new(replies));
    let store = Arc::new(MessageStore::open(":memory:", 200).unwrap());
    let memory = Arc::new(megumi_agent::MemoryStore::open(":memory:").unwrap());
    let traces = Arc::new(TraceSink::open(":memory:", 500).unwrap());
    let mut config = AgentConfig::for_test();
    config.group_window = group_window;
    config.max_tool_iterations = 3;
    let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(SearchHistory::new(
        Arc::clone(&store),
        config.clone(),
    ))];
    let agent = Agent::new(
        store,
        memory,
        traces,
        llm.clone(),
        Arc::new(ToolRegistry::new(tools)),
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
        timestamp: chrono::Utc::now(),
    }
}

/// A scripted reply that calls `search_history` with `query`.
fn history_call(query: &str) -> LlmResponse {
    LlmResponse {
        tool_calls: vec![LlmToolCall {
            id: "call_1".into(),
            name: "search_history".into(),
            arguments: format!(r#"{{"query":"{query}"}}"#),
        }],
        ..Default::default()
    }
}

fn text_reply(text: &str) -> LlmResponse {
    LlmResponse {
        text: text.into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn a_message_outside_the_window_is_reachable_through_the_tool() {
    // The window is two messages, so the oldest is not in the prompt at all.
    let (agent, llm) = agent(
        vec![history_call("launch code"), text_reply("It is ALPHA-9.")],
        2,
    );
    let chat = ChatId::new("gA");

    // The fact is stated early, then buried by newer chatter.
    for (id, text) in [
        ("m0", "the launch code is ALPHA-9"),
        ("m1", "just chatting"),
        ("m2", "more chatter"),
    ] {
        let mut e = event("gA", ChatType::Group, "u1", text);
        e.message_id = id.into();
        agent.ingest(&e).unwrap();
    }
    assert_eq!(agent.store().len(&chat).unwrap(), 3);

    // A triggering message: the prompt shows only the last two, so the fact is
    // not visible to the model unless it asks.
    let mut trigger = event("gA", ChatType::Group, "u1", "@bot what is the launch code?");
    trigger.message_id = "m3".into();
    agent.ingest(&trigger).unwrap();
    agent.respond(&trigger).await.unwrap();

    let requests = llm.requests();
    assert_eq!(requests.len(), 2, "one call to use the tool, one to answer");
    // The prompt itself does not carry the buried message...
    assert!(
        !requests[0].user.contains("ALPHA-9"),
        "the prompt should not show the out-of-window message: {}",
        requests[0].user
    );
    assert!(!requests[0].tools.is_empty(), "the tool was advertised");
    assert_eq!(requests[0].tools[0].name, "search_history");
    // ...but the tool result does, and it reaches the model's second call.
    let results = requests[1]
        .messages
        .iter()
        .find_map(|message| match message {
            LlmMessage::ToolResults(results) => Some(results),
            _ => None,
        })
        .expect("the second request carries the tool results");
    assert!(
        results
            .iter()
            .any(|result| result.content.contains("ALPHA-9")),
        "{results:?}"
    );

    let trace = agent.traces().recent(1).unwrap().pop().unwrap();
    assert_eq!(trace.tool_calls.len(), 1);
    assert_eq!(trace.tool_calls[0].name, "search_history");
    assert!(trace.tool_calls[0].result.contains("ALPHA-9"));
}

#[tokio::test]
async fn history_search_cannot_read_another_chat() {
    // A private chat's message must not be reachable from a group turn, even
    // though both live in the same store.
    let (agent, llm) = agent(
        vec![history_call("secret"), text_reply("I don't know.")],
        30,
    );
    let mut private = event("dm", ChatType::Private, "u2", "CANARY-private-secret");
    private.message_id = "dm1".into();
    agent.ingest(&private).unwrap();

    let trigger = event("gA", ChatType::Group, "u1", "@bot what is the secret?");
    agent.ingest(&trigger).unwrap();
    agent.respond(&trigger).await.unwrap();

    // Neither the tool result nor anything the model saw carries the canary.
    let trace = agent.traces().recent(1).unwrap().pop().unwrap();
    assert_eq!(trace.tool_calls.len(), 1);
    assert!(
        !trace.tool_calls[0].result.contains("CANARY"),
        "{}",
        trace.tool_calls[0].result
    );
    let shown = llm.requests().pop().unwrap().messages;
    assert!(!format!("{shown:?}").contains("CANARY"), "{shown:?}");
}
