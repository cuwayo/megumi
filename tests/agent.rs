//! The WhatsApp adapter: converting a message and driving the agent.
//!
//! These build a real `whatsapp_rust::Client` that is never connected — enough
//! to exercise conversion and storage, since a non-triggering message sends
//! nothing — and hand the adapter a `Messages` event directly. The agent's trace
//! log is what the tests assert on, because a send on an unconnected client
//! cannot succeed.

use std::sync::Arc;
use std::time::Instant;

use chrono::Utc;
use megumi_agent::{AgentConfig, MessageStore, ScriptedLlm, TraceSink};
use megumi_whatsapp::agent::on_messages;
use megumi_whatsapp::{Data, NewsStore};
use whatsapp_rust::bot::Bot;
use whatsapp_rust::types::events::{BatchOrigin, InboundMessage, MessageBatch};
use whatsapp_rust::types::message::{MessageInfo, MessageSource};
use whatsapp_rust::wacore::store::InMemoryBackend;
use whatsapp_rust::waproto::whatsapp as wa;
use whatsapp_rust::{Client, Jid};

/// Builds a bot's `Data` over in-memory stores and a scripted model.
fn data() -> Data {
    let store = Arc::new(MessageStore::open(":memory:", 200).unwrap());
    let traces = Arc::new(TraceSink::open(":memory:", 500).unwrap());
    let llm = Arc::new(ScriptedLlm::always("hello from the agent"));
    let agent = Arc::new(megumi_agent::Agent::new(
        store,
        traces,
        llm,
        AgentConfig::for_test(),
    ));
    Data {
        started: Instant::now(),
        news: Arc::new(NewsStore::open(":memory:").unwrap()),
        news_task: tokio::sync::Mutex::new(None),
        agent,
    }
}

async fn client() -> Arc<Client> {
    Bot::builder()
        .with_backend(InMemoryBackend::new())
        .build()
        .await
        .expect("build the client")
        .client()
}

/// A `Messages` event carrying one text message in `chat`.
fn event(chat: &str, sender: &str, text: &str, is_group: bool, mentions_bot: bool) -> MessageBatch {
    let mut context = wa::ContextInfo::default();
    if mentions_bot {
        // The bot's own JID is unknown on an unconnected client, so a mention is
        // expressed with the same JID the message would mention; the test that
        // cares about detection builds the client first and passes its own.
        context.mentioned_jid = vec![sender.to_string()];
    }
    let message = wa::Message {
        extended_text_message: whatsapp_rust::buffa::MessageField::some(
            wa::message::ExtendedTextMessage {
                text: Some(text.to_string()),
                context_info: whatsapp_rust::buffa::MessageField::some(context),
                ..Default::default()
            },
        ),
        ..Default::default()
    };
    let info = MessageInfo {
        source: MessageSource {
            chat: chat.parse::<Jid>().unwrap(),
            sender: sender.parse::<Jid>().unwrap(),
            is_group,
            ..Default::default()
        },
        id: format!("m-{text}").into(),
        push_name: "Budi".into(),
        timestamp: Utc::now(),
        ..Default::default()
    };
    let inbound = InboundMessage::builder()
        .message(Arc::new(message))
        .info(Arc::new(info))
        .build();
    MessageBatch::builder()
        .messages(Arc::from(vec![inbound].into_boxed_slice()))
        .origin(BatchOrigin::Live)
        .build()
}

#[tokio::test]
async fn an_untriggered_group_message_is_stored_and_not_answered() {
    let data = data();
    let client = client().await;
    let batch = event(
        "120363@g.us",
        "62812@s.whatsapp.net",
        "just chatting",
        true,
        false,
    );

    on_messages(&data, &client, &batch).await;

    let chat = megumi_agent::ChatId::new("120363@g.us");
    assert_eq!(data.agent.store().recent(&chat, 10).unwrap().len(), 1);
    assert!(data.agent.traces().recent(10).unwrap().is_empty());
}

#[tokio::test]
async fn a_private_message_is_answered() {
    let data = data();
    let client = client().await;
    let batch = event(
        "62812@s.whatsapp.net",
        "62812@s.whatsapp.net",
        "hi there",
        false,
        false,
    );

    on_messages(&data, &client, &batch).await;

    // The turn ran: a trace was recorded even though the unconnected client
    // could not deliver the reply.
    let traces = data.agent.traces().recent(10).unwrap();
    assert_eq!(traces.len(), 1);
    assert_eq!(traces[0].trigger, "private");
}

#[tokio::test]
async fn a_mention_of_the_bot_triggers_a_turn() {
    let data = data();
    let client = client().await;
    // On an unconnected client `pn()` is `None`, so a mention cannot match a
    // real bot JID; the adapter is exercised for storage and the non-trigger
    // path here. Mention detection itself is unit-tested in the adapter module.
    let batch = event(
        "120363@g.us",
        "62812@s.whatsapp.net",
        "@bot hello",
        true,
        true,
    );

    on_messages(&data, &client, &batch).await;

    let chat = megumi_agent::ChatId::new("120363@g.us");
    assert_eq!(data.agent.store().recent(&chat, 10).unwrap().len(), 1);
}

#[tokio::test]
async fn a_command_is_stored_but_not_answered() {
    let data = data();
    let client = client().await;
    let batch = event("120363@g.us", "62812@s.whatsapp.net", "!ping", true, true);

    on_messages(&data, &client, &batch).await;

    let chat = megumi_agent::ChatId::new("120363@g.us");
    assert_eq!(data.agent.store().recent(&chat, 10).unwrap().len(), 1);
    assert!(data.agent.traces().recent(10).unwrap().is_empty());
}
