//! End-to-end cost of one framework dispatch, and how much it allocates.
//!
//! Each iteration runs the whole [`Framework::dispatch_event`] path — user-data
//! resolution, the per-event `Framework::clone`, `MessageContext` construction,
//! prefix/alias routing, the gates, the command body with its hooks, and the
//! event hook — against a real `whatsapp_rust::Client` built by `Bot::builder`
//! and never connected, so no network is involved.
//!
//! Run with `cargo bench -p megumi-framework`. This is a plain `harness = false`
//! binary rather than a criterion bench so the crate takes on no new dependency.

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use megumi::{Context, Error, Event, Framework, FrameworkContext, command};
use whatsapp_rust::bot::Bot;
use whatsapp_rust::types::events::{BatchOrigin, InboundMessage, MessageBatch};
use whatsapp_rust::types::message::MessageInfo;
use whatsapp_rust::wacore::store::InMemoryBackend;
use whatsapp_rust::waproto::whatsapp as wa;

/// Counts allocations so the bench can report the per-dispatch heap cost, which
/// is the memory half of the question the dispatch path has to answer.
struct Counting;

static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// The state the command and the event hook touch, so both halves of dispatch
/// do real work rather than being optimised away.
struct Data {
    hits: AtomicU64,
}

#[command(name = "ping", aliases("p"))]
async fn ping(ctx: Context<Data>) -> Result<(), Error> {
    ctx.data().hits.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

fn event_hook(
    ctx: FrameworkContext<Data>,
    _event: Arc<Event>,
) -> megumi::BoxFuture<Result<(), Error>> {
    Box::pin(async move {
        ctx.data.hits.fetch_add(1, Ordering::Relaxed);
        Ok(())
    })
}

async fn client() -> Arc<whatsapp_rust::Client> {
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

fn framework() -> Framework<Data> {
    Framework::builder()
        .setup(|_client| async move {
            Ok(Data {
                hits: AtomicU64::new(0),
            })
        })
        .prefix("!")
        .event_handler(event_hook)
        .commands([ping()])
        .build()
}

/// The same framework without an event handler, to show what the hook costs on
/// top of the command path.
fn framework_without_hook() -> Framework<Data> {
    Framework::builder()
        .setup(|_client| async move {
            Ok(Data {
                hits: AtomicU64::new(0),
            })
        })
        .prefix("!")
        .commands([ping()])
        .build()
}

/// Times `iterations` dispatches of `event`, after warming the user-data cell so
/// the number is the steady state rather than the one-off setup.
async fn time(
    framework: &Framework<Data>,
    client: &Arc<whatsapp_rust::Client>,
    event: &Arc<Event>,
    iterations: u32,
) -> f64 {
    framework
        .dispatch_event(Arc::clone(client), Arc::clone(event))
        .await;

    let start = Instant::now();
    for _ in 0..iterations {
        framework
            .dispatch_event(Arc::clone(client), Arc::clone(event))
            .await;
    }
    let elapsed = start.elapsed();
    black_box(elapsed);
    elapsed.as_nanos() as f64 / f64::from(iterations)
}

/// Allocations for one dispatch, measured over `iterations` runs.
async fn allocations(
    framework: &Framework<Data>,
    client: &Arc<whatsapp_rust::Client>,
    event: &Arc<Event>,
    iterations: u32,
) -> (f64, f64) {
    framework
        .dispatch_event(Arc::clone(client), Arc::clone(event))
        .await;

    let before = (
        ALLOCATIONS.load(Ordering::Relaxed),
        BYTES.load(Ordering::Relaxed),
    );
    for _ in 0..iterations {
        framework
            .dispatch_event(Arc::clone(client), Arc::clone(event))
            .await;
    }
    let allocations = ALLOCATIONS.load(Ordering::Relaxed) - before.0;
    let bytes = BYTES.load(Ordering::Relaxed) - before.1;
    (
        allocations as f64 / f64::from(iterations),
        bytes as f64 / f64::from(iterations),
    )
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    const ITERATIONS: u32 = 200_000;
    let client = client().await;
    let framework = framework();

    let command = message_event("!ping");
    let plain = message_event("just chatting, no command here");
    let unknown = message_event("!nosuchcommand");

    println!("one full dispatch_event, release build\n");
    for (label, event) in [
        ("!ping (routed command)", &command),
        ("plain message (not a command)", &plain),
        ("!unknown (unknown command)", &unknown),
    ] {
        let ns = time(&framework, &client, event, ITERATIONS).await;
        let (allocs, bytes) = allocations(&framework, &client, event, ITERATIONS).await;
        println!("{label:<32} {ns:>8.0} ns/event   {allocs:>5.2} allocs   {bytes:>7.0} bytes");
    }

    // Isolates the event handler's cost on the command path.
    let no_hook = framework_without_hook();
    let ns = time(&no_hook, &client, &command, ITERATIONS).await;
    let (allocs, bytes) = allocations(&no_hook, &client, &command, ITERATIONS).await;
    println!(
        "{:<32} {ns:>8.0} ns/event   {allocs:>5.2} allocs   {bytes:>7.0} bytes",
        "!ping (no event handler)"
    );
}
