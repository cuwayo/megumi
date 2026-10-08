//! The morning news digest.
//!
//! A group admin turns it on with `!group news on`. Once a morning, for every
//! group that asked, the bot fetches a handful of headlines and posts them. The
//! subscription and the record of what has already gone out live in [`store`], so
//! both survive a restart.

mod feed;
mod retry;
mod schedule;
pub mod store;

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::NaiveDate;
use megumi::{BoxFuture, Error, Event, FrameworkContext};
use tracing::{info, warn};
use whatsapp_rust::Client;

use crate::data::Data;
use crate::news::store::NewsStore;
use retry::Backoff;

pub use schedule::morning_of;

/// How often the loop wakes to see whether a morning has started.
const TICK: Duration = Duration::from_secs(60);

/// Reacts to the connection lifecycle by starting or stopping the digest loop.
///
/// This is the framework's [`EventHook`](megumi::EventHook): [`crate::framework`]
/// registers it with `.event_handler(news::event_handler)` rather than wiring
/// `BotBuilder::on_connected` by hand. On `Connected` it stops the previous
/// connection's loop before starting its replacement, so a reconnect never
/// leaves two loops running. On `Disconnected` or `LoggedOut` it stops the loop,
/// so a dropped or unlinked session does not leave a loop ticking against a dead
/// client (and holding it alive) until the process ends.
pub fn event_handler(
    ctx: FrameworkContext<Data>,
    event: Arc<Event>,
) -> BoxFuture<Result<(), Error>> {
    Box::pin(async move {
        match &*event {
            Event::Connected(_) => {
                info!("Connected with WhatsApp");
                let next = tokio::spawn(run(ctx.client.clone(), Arc::clone(&ctx.data.news)));
                replace_digest(&ctx.data.news_task, Some(next)).await;
            }
            Event::Disconnected(_) | Event::LoggedOut(_) => {
                replace_digest(&ctx.data.news_task, None).await;
            }
            _ => {}
        }
        Ok(())
    })
}

/// Swaps the digest loop for `next`, stopping the running one first and waiting
/// for it to finish so the two never overlap.
///
/// The whole swap is one critical section, so two `Connected` events arriving
/// together cannot both start a loop.
async fn replace_digest(
    task: &tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    next: Option<tokio::task::JoinHandle<()>>,
) {
    let mut guard = task.lock().await;
    if let Some(previous) = guard.take() {
        previous.abort();
        let _ = previous.await;
    }
    *guard = next;
}

/// Posts the morning digest to every group that asked for it.
///
/// One loop per connection: [`event_handler`] starts it when WhatsApp connects,
/// stopping the previous connection's loop first so only one ever runs. Each
/// tick checks whether a new morning has begun; a morning's
/// digest is fetched once and sent to each subscribed group that has not already
/// had it. A group is marked done only after its message goes out, so a failure
/// is retried on a later tick and, since the record is on disk, a digest that
/// already went out is never posted a second time. A failed fetch is retried on
/// an increasing delay (see [`retry`]) so an unreachable feed is not asked every
/// tick for the rest of the morning.
pub async fn run(client: Arc<Client>, news: Arc<NewsStore>) {
    let http = reqwest::Client::builder()
        .user_agent("megumi-whatsapp")
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let mut backoff = Backoff::default();
    loop {
        if let Some(day) = morning_of(chrono::Local::now()) {
            deliver(&client, &news, &http, day, &mut backoff).await;
        }
        tokio::time::sleep(TICK).await;
    }
}

/// Sends `day`'s digest to every subscribed group still waiting for it.
async fn deliver(
    client: &Client,
    news: &NewsStore,
    http: &reqwest::Client,
    day: NaiveDate,
    backoff: &mut Backoff,
) {
    let chats = match news.enabled_chats() {
        Ok(chats) => chats,
        Err(error) => {
            warn!(%error, "could not read the news subscriptions");
            return;
        }
    };

    let pending: Vec<String> = chats
        .into_iter()
        .filter(|chat| match news.was_delivered(chat, day) {
            Ok(delivered) => !delivered,
            Err(error) => {
                warn!(%error, chat, "could not read the news delivery record");
                false
            }
        })
        .collect();
    if pending.is_empty() {
        return;
    }

    // One fetch for every group. Nothing is marked sent until a message lands,
    // so a failed morning is simply retried, but on an increasing delay rather
    // than every tick.
    if !backoff.ready(day, Instant::now()) {
        return;
    }
    let digest = match feed::fetch_digest(http).await {
        Ok(digest) => {
            backoff.record_success();
            digest
        }
        Err(error) => {
            warn!(%error, "the morning news could not be fetched");
            backoff.record_failure(day, Instant::now());
            return;
        }
    };

    for chat in &pending {
        let jid = match chat.parse::<whatsapp_rust::Jid>() {
            Ok(jid) => jid,
            Err(error) => {
                warn!(chat, %error, "skipping a news subscription with a bad chat id");
                continue;
            }
        };

        match client.send_text(jid, digest.clone()).await {
            Ok(_) => {
                if let Err(error) = news.mark_delivered(chat, day) {
                    warn!(chat, %error, "the digest was sent but could not be recorded");
                }
                info!(chat, "sent the morning news");
            }
            Err(error) => warn!(chat, %error, "failed to send the morning news"),
        }
    }
}
