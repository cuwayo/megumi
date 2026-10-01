//! The morning news digest.
//!
//! A group admin turns it on with `!group news on`. Once a morning, for every
//! group that asked, the bot fetches a handful of headlines and posts them. The
//! subscription and the record of what has already gone out live in [`store`], so
//! both survive a restart.

mod feed;
mod schedule;
pub mod store;

use std::sync::Arc;
use std::time::Duration;

use chrono::NaiveDate;
use tracing::{info, warn};
use whatsapp_rust::Client;

use crate::data::Data;

pub use schedule::morning_of;

/// How often the loop wakes to see whether a morning has started.
const TICK: Duration = Duration::from_secs(60);

/// Posts the morning digest to every group that asked for it.
///
/// One loop per connection: `main` starts it when WhatsApp connects and aborts it
/// on shutdown. Each tick checks whether a new morning has begun; a morning's
/// digest is fetched once and sent to each subscribed group that has not already
/// had it. A group is marked done only after its message goes out, so a failure
/// is retried on a later tick and, since the record is on disk, a digest that
/// already went out is never posted a second time.
pub async fn run(client: Arc<Client>, data: Data) {
    let http = reqwest::Client::builder()
        .user_agent("megumi-whatsapp")
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    loop {
        if let Some(day) = morning_of(chrono::Local::now()) {
            deliver(&client, &data, &http, day).await;
        }
        tokio::time::sleep(TICK).await;
    }
}

/// Sends `day`'s digest to every subscribed group still waiting for it.
async fn deliver(client: &Client, data: &Data, http: &reqwest::Client, day: NaiveDate) {
    let chats = match data.news.enabled_chats() {
        Ok(chats) => chats,
        Err(error) => {
            warn!(%error, "could not read the news subscriptions");
            return;
        }
    };

    let pending: Vec<String> = chats
        .into_iter()
        .filter(|chat| match data.news.was_delivered(chat, day) {
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
    // so a failed morning is simply retried next tick.
    let digest = match feed::fetch_digest(http).await {
        Ok(digest) => digest,
        Err(error) => {
            warn!(%error, "the morning news could not be fetched");
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
                if let Err(error) = data.news.mark_delivered(chat, day) {
                    warn!(chat, %error, "the digest was sent but could not be recorded");
                }
                info!(chat, "sent the morning news");
            }
            Err(error) => warn!(chat, %error, "failed to send the morning news"),
        }
    }
}
