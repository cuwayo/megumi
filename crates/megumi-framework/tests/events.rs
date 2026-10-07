//! The event-dispatch path: lazy `setup`, message dispatch, and the event hook.
//!
//! These drive the real [`Framework::dispatch_event`] with a real
//! `whatsapp_rust::Client` built by `Bot::builder`, which wires every cache and
//! background loop but never opens a connection. That is enough to exercise
//! dispatch without a live WhatsApp session: a command that only touches shared
//! state runs to completion with no network.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use megumi::{BoxFuture, Context, Error, Event, Framework, FrameworkContext, NoData, command};
use whatsapp_rust::bot::Bot;
use whatsapp_rust::types::events::{BatchOrigin, Connected, InboundMessage, MessageBatch};
use whatsapp_rust::types::message::MessageInfo;
use whatsapp_rust::wacore::store::InMemoryBackend;
use whatsapp_rust::waproto::whatsapp as wa;

/// The state these tests share between a command and the event hook, so a test
/// can observe both without sending a reply (which would need a connection).
struct Data {
    invocations: Arc<AtomicU64>,
    events: Arc<AtomicU64>,
    log: Arc<Mutex<Vec<&'static str>>>,
}

/// Increments the shared counter and records that the command ran. It sends
/// nothing, so it completes without a connection.
#[command(name = "counted")]
async fn counted(ctx: Context<Data>) -> Result<(), Error> {
    ctx.data().invocations.fetch_add(1, Ordering::Relaxed);
    ctx.data().log.lock().expect("log mutex").push("command");
    Ok(())
}

/// Records that the event hook ran, and for which kind of event.
fn record_event(ctx: FrameworkContext<Data>, _event: Arc<Event>) -> BoxFuture<Result<(), Error>> {
    Box::pin(async move {
        ctx.data.events.fetch_add(1, Ordering::Relaxed);
        ctx.data.log.lock().expect("log mutex").push("event");
        Ok(())
    })
}

/// A real client that is built but never connected, with its own in-memory
/// session store so parallel tests do not share state and nothing hits disk.
async fn unconnected_client() -> Arc<whatsapp_rust::Client> {
    Bot::builder()
        .with_backend(InMemoryBackend::new())
        .build()
        .await
        .expect("build the client")
        .client()
}

/// A `Messages` event carrying one text message.
fn message_event(text: &str) -> Arc<Event> {
    let message = wa::Message {
        conversation: Some(text.to_string()),
        ..Default::default()
    };
    let inbound = InboundMessage::builder()
        .message(Arc::new(message))
        .info(Arc::new(MessageInfo::default()))
        .build();
    let batch = MessageBatch::builder()
        .messages(Arc::from(vec![inbound].into_boxed_slice()))
        .origin(BatchOrigin::Live)
        .build();
    Arc::new(Event::Messages(batch))
}

/// A framework plus the counters its `setup` and command/hook write to, so a
/// test can observe dispatch without a connection.
struct Observed {
    framework: Framework<Data>,
    setups: Arc<AtomicU64>,
    invocations: Arc<AtomicU64>,
    events: Arc<AtomicU64>,
    log: Arc<Mutex<Vec<&'static str>>>,
}

/// Builds a framework whose `setup` records how many times it ran, and whose
/// shared `Data` the counters observe.
fn observed_framework() -> Observed {
    let setups = Arc::new(AtomicU64::new(0));
    let invocations = Arc::new(AtomicU64::new(0));
    let events = Arc::new(AtomicU64::new(0));
    let log = Arc::new(Mutex::new(Vec::new()));

    let setups_in = Arc::clone(&setups);
    let inv = Arc::clone(&invocations);
    let ev = Arc::clone(&events);
    let log_in = Arc::clone(&log);
    let framework = Framework::builder()
        .setup(move |_client| {
            setups_in.fetch_add(1, Ordering::Relaxed);
            let (inv, ev, log) = (Arc::clone(&inv), Arc::clone(&ev), Arc::clone(&log_in));
            async move {
                Ok(Data {
                    invocations: inv,
                    events: ev,
                    log,
                })
            }
        })
        .prefix("!")
        .event_handler(record_event)
        .commands([counted()])
        .build();

    Observed {
        framework,
        setups,
        invocations,
        events,
        log,
    }
}

#[tokio::test]
async fn setup_runs_once_and_commands_reach_the_data() {
    let client = unconnected_client().await;
    let observed = observed_framework();

    observed
        .framework
        .dispatch_event(Arc::clone(&client), message_event("!counted"))
        .await;
    observed
        .framework
        .dispatch_event(Arc::clone(&client), message_event("!counted"))
        .await;

    assert_eq!(
        observed.setups.load(Ordering::Relaxed),
        1,
        "setup must run exactly once, however many events arrive"
    );
    assert_eq!(
        observed.invocations.load(Ordering::Relaxed),
        2,
        "both command messages must reach the command body"
    );
    assert_eq!(
        observed.events.load(Ordering::Relaxed),
        2,
        "the event hook must run for every event, messages included"
    );
}

#[tokio::test]
async fn the_event_hook_runs_for_non_message_events() {
    let client = unconnected_client().await;
    let observed = observed_framework();

    observed
        .framework
        .dispatch_event(
            Arc::clone(&client),
            Arc::new(Event::Connected(Connected::builder().build())),
        )
        .await;

    assert_eq!(
        observed.events.load(Ordering::Relaxed),
        1,
        "a Connected event must still reach the event hook"
    );
    assert_eq!(
        observed.invocations.load(Ordering::Relaxed),
        0,
        "a non-message event must not run a command"
    );
}

#[tokio::test]
async fn messages_are_dispatched_before_the_event_hook() {
    let client = unconnected_client().await;
    let observed = observed_framework();

    observed
        .framework
        .dispatch_event(client, message_event("!counted"))
        .await;

    assert_eq!(
        *observed.log.lock().expect("log mutex"),
        vec!["command", "event"],
        "the command must run before the event hook sees the event"
    );
}

#[tokio::test]
async fn a_failed_setup_drops_events_without_panicking() {
    let client = unconnected_client().await;
    // No `Data` is ever produced, so a dispatched command must be dropped, not
    // run against a context whose `data()` would panic.
    let framework = Framework::builder()
        .setup(|_client| async move { Err("setup failed".into()) })
        .prefix("!")
        .commands([counted()])
        .build();

    framework
        .dispatch_event(client, message_event("!counted"))
        .await;
}

#[tokio::test]
async fn a_framework_without_setup_dispatches_with_no_data() {
    // The `NoData` default path: no setup closure at all, so the data is ready
    // from the start and a plain command still dispatches.
    let client = unconnected_client().await;
    let framework = Framework::builder()
        .prefix("!")
        .event_handler(|_ctx: FrameworkContext<NoData>, _event| Box::pin(async { Ok(()) }))
        .build();

    framework
        .dispatch_event(
            client,
            Arc::new(Event::Connected(Connected::builder().build())),
        )
        .await;
}
