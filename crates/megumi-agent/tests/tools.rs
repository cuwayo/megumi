//! The milestone-5 exit criteria: the bounded, traced tool loop.
//!
//! These drive the real [`Agent`] with a scripted model whose replies include
//! tool calls, and assert on what the model was *shown* — the transcript it
//! received and the tool results the loop produced — rather than on its prose.
//! No network is involved: `search_memory` runs against the in-memory store, and
//! `web_search` is only ever exercised as a spec (its own request shape is
//! unit-tested in the module).

use std::sync::Arc;

use megumi_agent::{
    Agent, AgentConfig, ChatId, ChatType, InboundEvent, LlmMessage, LlmResponse, LlmToolCall,
    MemoryOp, MemoryStore, MessageStore, ScriptedLlm, SenderId, Tool, ToolRegistry, ToolSpec,
    TraceSink, Visibility,
};

/// Builds an agent with tools on and a registry of `tools`.
///
/// `max_tool_iterations` is raised from `for_test`'s 0, so the loop runs.
fn agent(
    replies: Vec<LlmResponse>,
    tools: Vec<Arc<dyn Tool>>,
    max_tool_iterations: usize,
) -> (Agent, Arc<ScriptedLlm>) {
    let llm = Arc::new(ScriptedLlm::new(replies));
    let store = Arc::new(MessageStore::open(":memory:", 200).unwrap());
    let memory = Arc::new(MemoryStore::open(":memory:").unwrap());
    let traces = Arc::new(TraceSink::open(":memory:", 500).unwrap());
    let mut config = AgentConfig::for_test();
    config.max_tool_iterations = max_tool_iterations;
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

/// A scripted reply that calls `search_memory` with `query`.
fn memory_call(query: &str) -> LlmResponse {
    LlmResponse {
        tool_calls: vec![LlmToolCall {
            id: "call_1".into(),
            name: "search_memory".into(),
            arguments: format!(r#"{{"query":"{query}"}}"#),
        }],
        ..Default::default()
    }
}

fn text_reply(text: &str) -> LlmResponse {
    LlmResponse {
        text: text.into(),
        input_tokens: Some(10),
        output_tokens: Some(4),
        ..Default::default()
    }
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

fn fact(content: &str, visibility: Visibility) -> megumi_agent::memory::store::NewFact {
    megumi_agent::memory::store::NewFact {
        content: content.into(),
        visibility,
        subject: None,
        confidence: 0.9,
        importance: 0.7,
        valid_from: chrono::Utc::now(),
        evidence: vec!["m1".into()],
    }
}

#[tokio::test]
async fn a_tool_call_is_run_and_its_result_answers_the_turn() {
    // First reply calls the tool; second answers using the result.
    let (agent, llm) = agent(
        vec![memory_call("venue"), text_reply("It is the old hall.")],
        Vec::new(),
        3,
    );
    agent
        .memory()
        .apply(
            &ChatId::new("gA"),
            &[MemoryOp::Add(fact(
                "The team's venue is the old hall.",
                Visibility::Chat,
            ))],
            None,
        )
        .unwrap();

    let trigger = event("gA", ChatType::Group, "u1", "@bot where is the venue?");
    agent.ingest(&trigger).unwrap();
    let action = agent.respond(&trigger).await.unwrap();
    assert!(matches!(
        action,
        Some(megumi_agent::OutboundAction::SendText { .. })
    ));

    // The loop ran the tool and fed the fact back: the second request carries a
    // tool result whose content is the stored fact.
    let requests = llm.requests();
    assert_eq!(requests.len(), 2, "one call to use the tool, one to answer");
    assert!(!requests[0].tools.is_empty(), "the tool was advertised");
    assert_eq!(requests[0].tools[0].name, "search_memory");
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
            .any(|result| result.content.contains("old hall")),
        "{results:?}"
    );

    // The call is recorded in the trace, with its arguments and result.
    let trace = agent.traces().recent(1).unwrap().pop().unwrap();
    assert_eq!(trace.tool_calls.len(), 1);
    assert_eq!(trace.tool_calls[0].name, "search_memory");
    assert!(trace.tool_calls[0].arguments.contains("venue"));
    assert!(trace.tool_calls[0].result.contains("old hall"));
    // Tokens are summed across both calls.
    assert_eq!(trace.input_tokens, Some(10));
}

#[tokio::test]
async fn the_loop_is_bounded_and_never_spins() {
    // A model that always asks for a tool must be cut off at the bound, not
    // loop forever. With the bound at 1, the model gets one chance to call; the
    // second call is the final turn and its text (empty) ends the turn.
    let replies = vec![memory_call("x"), memory_call("x"), text_reply("done")];
    let (agent, llm) = agent(replies, Vec::new(), 1);
    let trigger = event("gA", ChatType::Group, "u1", "@bot hi");
    agent.ingest(&trigger).unwrap();
    let _ = agent.respond(&trigger).await.unwrap();

    // 0..=1 is two model calls: the first returns a call, the second is the last
    // iteration and its call is not run, so the turn ends with no text.
    assert_eq!(llm.requests().len(), 2);
}

#[tokio::test]
async fn a_tool_call_is_not_run_past_the_bound() {
    // Every iteration returns a call, so the turn exhausts the bound. It must
    // stop after max_tool_iterations + 1 calls and record the calls it ran.
    let replies = vec![memory_call("a"), memory_call("b"), memory_call("c")];
    let (agent, llm) = agent(replies, Vec::new(), 2);
    let trigger = event("gA", ChatType::Group, "u1", "@bot hi");
    agent.ingest(&trigger).unwrap();
    assert!(agent.respond(&trigger).await.unwrap().is_none());

    // 0..=2 is three model calls; each but the last produced a call that ran, so
    // two tool calls are recorded and the third reply's call is left unrun.
    assert_eq!(llm.requests().len(), 3);
    let trace = agent.traces().recent(1).unwrap().pop().unwrap();
    assert_eq!(trace.tool_calls.len(), 2);
    assert!(trace.reply.is_none(), "no text means silence");
}

#[tokio::test]
async fn search_memory_cannot_cross_the_privacy_boundary() {
    // A private fact must not reach a group turn even through the tool.
    let (agent, llm) = agent(
        vec![memory_call("secret"), text_reply("I don't know.")],
        Vec::new(),
        3,
    );
    agent
        .memory()
        .apply(
            &ChatId::new("dm"),
            &[MemoryOp::Add(fact(
                "CANARY-private-dm-secret",
                Visibility::Private,
            ))],
            None,
        )
        .unwrap();

    let trigger = event("gA", ChatType::Group, "u1", "@bot what is the secret?");
    agent.ingest(&trigger).unwrap();
    agent.respond(&trigger).await.unwrap();

    // The group turn's tool result must not carry the private fact.
    let trace = agent.traces().recent(1).unwrap().pop().unwrap();
    assert_eq!(trace.tool_calls.len(), 1);
    assert!(
        !trace.tool_calls[0].result.contains("CANARY"),
        "{}",
        trace.tool_calls[0].result
    );
    // And it is not in what the model was shown either.
    let shown = llm.requests().pop().unwrap().messages;
    assert!(!format!("{shown:?}").contains("CANARY"), "{shown:?}");
}

/// A tool that always fails, to prove a failure is a result, not a turn failure.
struct FailingTool;

impl Tool for FailingTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "failing".into(),
            description: "always fails".into(),
            parameters: serde_json::json!({ "type": "object", "properties": {} }),
        }
    }

    fn call(
        &self,
        _arguments: &serde_json::Value,
    ) -> megumi_agent::llm::BoxFuture<Result<String, String>> {
        Box::pin(async { Err("the upstream service is down".to_string()) })
    }
}

#[tokio::test]
async fn a_failing_tool_returns_an_error_result_and_the_turn_still_answers() {
    let call = LlmResponse {
        tool_calls: vec![LlmToolCall {
            id: "c1".into(),
            name: "failing".into(),
            arguments: "{}".into(),
        }],
        ..Default::default()
    };
    let (agent, llm) = agent(
        vec![call, text_reply("Sorry, I couldn't check.")],
        vec![Arc::new(FailingTool)],
        3,
    );
    let trigger = event("gA", ChatType::Group, "u1", "@bot check something");
    agent.ingest(&trigger).unwrap();
    let action = agent.respond(&trigger).await.unwrap();
    assert!(matches!(
        action,
        Some(megumi_agent::OutboundAction::SendText { .. })
    ));

    let trace = agent.traces().recent(1).unwrap().pop().unwrap();
    assert_eq!(trace.tool_calls.len(), 1);
    assert!(
        trace.tool_calls[0]
            .result
            .contains("the upstream service is down"),
        "{}",
        trace.tool_calls[0].result
    );
    // The error was handed back to the model, which still produced a reply.
    let shown = llm.requests().pop().unwrap().messages;
    assert!(
        format!("{shown:?}").contains("upstream service"),
        "{shown:?}"
    );
}

#[tokio::test]
async fn tools_off_is_a_single_call_as_before() {
    let (agent, llm) = agent(vec![text_reply("hello")], Vec::new(), 0);
    let trigger = event("gA", ChatType::Group, "u1", "@bot hi");
    agent.ingest(&trigger).unwrap();
    agent.respond(&trigger).await.unwrap();

    let requests = llm.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].tools.is_empty(), "no tools advertised");
    // The single-call shape: one user turn, and it is the request's `user`.
    assert_eq!(requests[0].messages.len(), 1);
    assert_eq!(
        requests[0].messages[0],
        megumi_agent::LlmMessage::Text {
            assistant: false,
            text: requests[0].user.clone(),
        }
    );
}
