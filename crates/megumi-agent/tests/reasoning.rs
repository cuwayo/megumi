//! The milestone-8 exit criteria: the planner and the evaluator.
//!
//! These drive the real [`Agent`] with a scripted model and assert on what the
//! model was *shown* and what was sent — a plan reaching the prompt, a verdict
//! accepted or sent back for revision, a malformed verdict failing open — rather
//! than on the model's prose. No network is involved.
//!
//! The scripted replies come in call order: with planning on, the plan call is
//! first; then the turn's reply; then the evaluator's verdict; then, on a
//! revision, the rewritten reply and the verdict on it.

use std::sync::Arc;

use megumi_agent::{
    Agent, AgentConfig, ChatId, ChatType, InboundEvent, LlmResponse, MemoryOp, MemoryStore,
    MessageStore, OutboundAction, ScriptedLlm, SenderId, ToolRegistry, TraceSink, Visibility,
};

/// An agent whose config `for_test()` is adjusted by `tweak`.
fn agent(
    replies: impl IntoIterator<Item = &'static str>,
    tweak: impl FnOnce(&mut AgentConfig),
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
    tweak(&mut config);
    // `search_memory` is registered over the agent's own memory store, the way
    // the bot wires it, so a turn with tools on actually offers one — the
    // contrast the tool-free reasoning calls are asserted against.
    let tools: Vec<Arc<dyn megumi_agent::Tool>> = vec![Arc::new(megumi_agent::SearchMemory::new(
        Arc::clone(&memory),
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

/// The plan-then-answer config: planning on, the evaluator off.
fn planning() -> impl FnOnce(&mut AgentConfig) {
    |config: &mut AgentConfig| {
        config.planner_enabled = true;
        config.plan_min_words = 3;
    }
}

/// The answer-then-evaluate config: planning off, one revision allowed.
fn evaluating() -> impl FnOnce(&mut AgentConfig) {
    |config: &mut AgentConfig| {
        config.max_revisions = 1;
    }
}

fn text_of(action: Option<OutboundAction>) -> Option<String> {
    action.map(|OutboundAction::SendText { text, .. }| text)
}

#[tokio::test]
async fn a_nontrivial_request_is_planned_and_the_plan_reaches_the_prompt() {
    // Call order: the plan, then the answer.
    let (agent, llm) = agent(
        [r#"["look up the population", "compare to Paris"]"#, "Paris"],
        planning(),
    );
    let trigger = event(
        "gA",
        ChatType::Group,
        "u1",
        "@bot which is the largest city in France?",
    );
    agent.ingest(&trigger).unwrap();
    agent.respond(&trigger).await.unwrap();

    let requests = llm.requests();
    assert_eq!(requests.len(), 2, "one plan call, one answer");
    // The plan call is asked to plan, not to answer.
    assert!(
        requests[0].user.contains("<message>"),
        "{}",
        requests[0].user
    );
    // The answer's prompt carries the plan as a layer.
    assert!(requests[1].user.contains("<plan>"), "{}", requests[1].user);
    assert!(
        requests[1].user.contains("look up the population"),
        "{}",
        requests[1].user
    );

    let trace = agent.traces().recent(1).unwrap().pop().unwrap();
    assert_eq!(
        trace.plan,
        vec![
            "look up the population".to_string(),
            "compare to Paris".to_string()
        ]
    );
}

#[tokio::test]
async fn a_short_request_is_not_planned() {
    // Planning is on, but `plan_min_words` stays at its default of twelve, so a
    // two-word request is below the bar.
    let (agent, llm) = agent(["hi there"], |config: &mut AgentConfig| {
        config.planner_enabled = true;
    });
    let trigger = event("gA", ChatType::Group, "u1", "@bot hi");
    agent.ingest(&trigger).unwrap();
    agent.respond(&trigger).await.unwrap();

    assert_eq!(llm.requests().len(), 1, "no plan call for a short request");
    let trace = agent.traces().recent(1).unwrap().pop().unwrap();
    assert!(trace.plan.is_empty());
}

#[tokio::test]
async fn an_accepted_draft_is_sent_unchanged() {
    let (agent, llm) = agent(
        [
            "The venue is the old hall.",
            r#"{"verdict":"ACCEPT","reason":"fine"}"#,
        ],
        evaluating(),
    );
    let trigger = event("gA", ChatType::Group, "u1", "@bot where is the venue?");
    agent.ingest(&trigger).unwrap();
    let action = agent.respond(&trigger).await.unwrap();

    assert_eq!(
        text_of(action).as_deref(),
        Some("The venue is the old hall.")
    );
    let trace = agent.traces().recent(1).unwrap().pop().unwrap();
    assert_eq!(trace.verdict.as_deref(), Some("ACCEPT"));
    assert_eq!(trace.revisions, 0);
    assert_eq!(llm.requests().len(), 2, "the draft and its verdict");
}

#[tokio::test]
async fn a_revise_verdict_rewrites_the_reply() {
    // Call order: draft, verdict, rewritten reply, verdict on it.
    let (agent, llm) = agent(
        [
            "It is somewhere.",
            r#"{"verdict":"REVISE","reason":"too vague"}"#,
            "The venue is the old hall.",
            r#"{"verdict":"ACCEPT","reason":"fine now"}"#,
        ],
        evaluating(),
    );
    let trigger = event("gA", ChatType::Group, "u1", "@bot where is the venue?");
    agent.ingest(&trigger).unwrap();
    let action = agent.respond(&trigger).await.unwrap();

    assert_eq!(
        text_of(action).as_deref(),
        Some("The venue is the old hall.")
    );
    let requests = llm.requests();
    assert_eq!(requests.len(), 4);
    // The revision call carries the draft and the critique, so the model knows
    // what to fix.
    let revision = &requests[2];
    let shown = format!("{:?}", revision.messages);
    assert!(shown.contains("It is somewhere."), "{shown}");
    assert!(shown.contains("too vague"), "{shown}");

    let trace = agent.traces().recent(1).unwrap().pop().unwrap();
    assert_eq!(trace.revisions, 1);
    assert_eq!(trace.verdict.as_deref(), Some("ACCEPT"));
}

#[tokio::test]
async fn a_malformed_verdict_fails_open() {
    // The evaluator's reply is not a verdict, so the draft stands — the
    // evaluator can only ever improve a reply, never suppress one.
    let (agent, llm) = agent(
        ["The venue is the old hall.", "sounds good to me"],
        evaluating(),
    );
    let trigger = event("gA", ChatType::Group, "u1", "@bot where is the venue?");
    agent.ingest(&trigger).unwrap();
    let action = agent.respond(&trigger).await.unwrap();

    assert_eq!(
        text_of(action).as_deref(),
        Some("The venue is the old hall.")
    );
    let trace = agent.traces().recent(1).unwrap().pop().unwrap();
    assert!(trace.verdict.is_none());
    assert_eq!(trace.revisions, 0);
    // No revision call was made: the draft and the unreadable verdict are all.
    assert_eq!(llm.requests().len(), 2);
}

#[tokio::test]
async fn a_silent_reply_is_never_evaluated() {
    let (agent, llm) = agent(["NO_REPLY"], evaluating());
    let trigger = event("gA", ChatType::Group, "u1", "@bot never mind");
    agent.ingest(&trigger).unwrap();
    let action = agent.respond(&trigger).await.unwrap();

    assert!(action.is_none());
    // Only the turn itself ran; a NO_REPLY has nothing to judge.
    assert_eq!(llm.requests().len(), 1);
    let trace = agent.traces().recent(1).unwrap().pop().unwrap();
    assert!(trace.verdict.is_none());
}

#[tokio::test]
async fn revisions_are_bounded() {
    // Every verdict asks for a revision; the budget of one stops it after a
    // single rewrite, so the turn cannot loop.
    let (agent, llm) = agent(
        [
            "draft",
            r#"{"verdict":"REVISE","reason":"again"}"#,
            "revised",
            r#"{"verdict":"REVISE","reason":"still"}"#,
        ],
        evaluating(),
    );
    let trigger = event("gA", ChatType::Group, "u1", "@bot do the thing");
    agent.ingest(&trigger).unwrap();
    let action = agent.respond(&trigger).await.unwrap();

    assert_eq!(text_of(action).as_deref(), Some("revised"));
    let trace = agent.traces().recent(1).unwrap().pop().unwrap();
    assert_eq!(trace.revisions, 1);
    // draft + verdict + revision + verdict, and no third rewrite.
    assert_eq!(llm.requests().len(), 4);
}

#[tokio::test]
async fn reasoning_is_off_under_for_test() {
    // The default test config makes no plan and no verdict: one call, one reply.
    let (agent, llm) = agent(["just the answer"], |_config| {});
    let trigger = event(
        "gA",
        ChatType::Group,
        "u1",
        "@bot which is the largest city in France?",
    );
    agent.ingest(&trigger).unwrap();
    let action = agent.respond(&trigger).await.unwrap();

    assert_eq!(text_of(action).as_deref(), Some("just the answer"));
    assert_eq!(llm.requests().len(), 1);
    let trace = agent.traces().recent(1).unwrap().pop().unwrap();
    assert!(trace.plan.is_empty());
    assert!(trace.verdict.is_none());
    assert_eq!(trace.revisions, 0);
}

#[tokio::test]
async fn a_reply_echoing_the_plan_layer_is_dropped() {
    // The output guard knows the new tag, so an injection that makes the model
    // echo the plan scaffolding is suppressed like any other internal tag.
    let (agent, _llm) = agent(["<plan>step one</plan>"], |_config| {});
    let trigger = event("gA", ChatType::Group, "u1", "@bot what is the plan?");
    agent.ingest(&trigger).unwrap();
    assert!(agent.respond(&trigger).await.unwrap().is_none());
}

#[tokio::test]
async fn the_reasoning_passes_do_not_read_memory() {
    // The plan and the verdict are built from the trigger and the draft alone —
    // no `<memories>` block, and no tools — so they add no privacy surface. The
    // revision is the one exception by design: it reuses the turn's own prompt,
    // so it carries exactly what the main turn the same reader already saw.
    let (agent, llm) = agent(
        [
            r#"["answer from the facts"]"#,
            "It is the old hall.",
            r#"{"verdict":"REVISE","reason":"be specific"}"#,
            "The venue is the old hall.",
            r#"{"verdict":"ACCEPT","reason":"good"}"#,
        ],
        |config: &mut AgentConfig| {
            config.planner_enabled = true;
            config.plan_min_words = 3;
            config.max_revisions = 1;
            // Advertise tools on the main turn, so the empty tool lists on the
            // reasoning calls prove they are tool-free by construction.
            config.max_tool_iterations = 3;
        },
    );
    agent
        .memory()
        .apply(
            &ChatId::new("gA"),
            &[MemoryOp::Add(megumi_agent::memory::store::NewFact {
                content: "The team's venue is the old hall.".into(),
                visibility: Visibility::Chat,
                subject: None,
                confidence: 0.9,
                importance: 0.7,
                valid_from: chrono::Utc::now(),
                evidence: vec!["m1".into()],
            })],
            None,
        )
        .unwrap();

    let trigger = event("gA", ChatType::Group, "u1", "@bot where is the venue?");
    agent.ingest(&trigger).unwrap();
    agent.respond(&trigger).await.unwrap();

    let requests = llm.requests();
    // plan, main, verdict, revision, verdict
    assert_eq!(requests.len(), 5);
    // The main turn sees the memory and is offered tools.
    assert!(
        requests[1].user.contains("<memories>"),
        "{}",
        requests[1].user
    );
    assert!(!requests[1].tools.is_empty(), "the main turn offers tools");
    // The plan and the two verdict calls see neither.
    for index in [0, 2, 4] {
        assert!(
            !requests[index].user.contains("<memories>"),
            "request {index} leaked memory: {}",
            requests[index].user
        );
        assert!(
            requests[index].tools.is_empty(),
            "request {index} advertised tools"
        );
    }
    // The revision reuses the turn's own prompt, so it matches the main turn.
    assert_eq!(requests[3].user, requests[1].user);
    assert!(requests[3].tools.is_empty(), "the revision offers no tools");
}
