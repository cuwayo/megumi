//! The weekly price update.
//!
//! A group admin turns it on with `!group price on`, and can watch more than one
//! symbol with `!group price add`. Once a week, for every group that asked, the
//! bot charts each of its symbols and posts the images. The subscriptions and the
//! record of what has already gone out live in [`store`], so both survive a
//! restart. `!price` renders the same chart on demand without waiting for the hour.

// `chart` and `market` are visible to the crate so `!price` can render the same
// chart on demand that the scheduler posts.
pub(crate) mod chart;
pub(crate) mod market;
mod models;
mod schedule;
pub mod store;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::NaiveDate;
use tracing::{info, warn};
use whatsapp_rust::Client;
use whatsapp_rust::download::MediaType;
use whatsapp_rust::media::ImageOptions;
use whatsapp_rust::upload::UploadOptions;

use crate::data::Data;
use crate::news::retry::Backoff;
use market::Quote;

pub use schedule::window_of;

/// How often the loop wakes to see whether the week's hour has arrived.
const TICK: Duration = Duration::from_secs(60);

/// Posts the weekly price update to every group that asked for it.
///
/// One loop per connection: `main` starts it when WhatsApp connects, stopping
/// the previous connection's loop first so only one ever runs. Each tick checks
/// whether this week's hour has begun; a group's symbols are charted once and
/// sent, and a group is marked done only after at least one image lands, so a
/// failure is retried on a later tick and a restart never reposts. A failed pass
/// is held back on an increasing delay (see [`crate::news::retry`]) so an
/// unreachable Yahoo Finance is not asked every tick.
pub async fn run(client: Arc<Client>, data: Data) {
    let http = reqwest::Client::builder()
        .user_agent("megumi-whatsapp")
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    let mut backoff = Backoff::default();
    loop {
        if let Some(week) = window_of(chrono::Local::now()) {
            deliver(&client, &data, &http, week, &mut backoff).await;
        }
        tokio::time::sleep(TICK).await;
    }
}

/// Charts and sends `week`'s update to every subscribed group still waiting.
async fn deliver(
    client: &Client,
    data: &Data,
    http: &reqwest::Client,
    week: NaiveDate,
    backoff: &mut Backoff,
) {
    let chats = match data.price.enabled_chats() {
        Ok(chats) => chats,
        Err(error) => {
            warn!(%error, "could not read the price subscriptions");
            return;
        }
    };

    let pending: Vec<String> = chats
        .into_iter()
        .filter(|chat| match data.price.was_delivered(chat, week) {
            Ok(delivered) => !delivered,
            Err(error) => {
                warn!(%error, chat, "could not read the price delivery record");
                false
            }
        })
        .collect();
    if pending.is_empty() {
        return;
    }

    // Nothing is marked sent until an image lands, so a failed pass is retried,
    // but on an increasing delay rather than every tick.
    if !backoff.ready(week, Instant::now()) {
        return;
    }

    // Two groups often watch the same symbol, so a pass fetches each one once.
    let mut cache: HashMap<String, Result<Quote, String>> = HashMap::new();
    let mut any_sent = false;

    for chat in &pending {
        let jid = match chat.parse::<whatsapp_rust::Jid>() {
            Ok(jid) => jid,
            Err(error) => {
                warn!(chat, %error, "skipping a price subscription with a bad chat id");
                continue;
            }
        };

        let symbols = match data.price.symbols(chat) {
            Ok(symbols) => symbols,
            Err(error) => {
                warn!(%error, chat, "could not read a group's price symbols");
                continue;
            }
        };

        let mut chat_sent = false;
        for symbol in symbols {
            let quote = match cached(http, &mut cache, &symbol).await {
                Ok(quote) => quote,
                Err(error) => {
                    warn!(chat, %symbol, %error, "could not fetch a price");
                    continue;
                }
            };

            match send_chart(client, jid.clone(), &quote).await {
                Ok(()) => {
                    chat_sent = true;
                    any_sent = true;
                }
                Err(error) => warn!(chat, %symbol, %error, "failed to send a price chart"),
            }
        }

        if chat_sent {
            if let Err(error) = data.price.mark_delivered(chat, week) {
                warn!(chat, %error, "the price update was sent but could not be recorded");
            }
            info!(chat, "sent the weekly price update");
        }
    }

    if any_sent {
        backoff.record_success();
    } else {
        backoff.record_failure(week, Instant::now());
    }
}

/// Fetches `symbol` through the pass cache, so each one is asked of Yahoo once.
async fn cached(
    http: &reqwest::Client,
    cache: &mut HashMap<String, Result<Quote, String>>,
    symbol: &str,
) -> Result<Quote, String> {
    if let Some(cached) = cache.get(symbol) {
        return cached.clone();
    }
    let fetched = market::fetch(http, symbol).await;
    cache.insert(symbol.to_string(), fetched.clone());
    fetched
}

/// Charts one quote and sends it to `jid`, with the summary as the caption.
async fn send_chart(client: &Client, jid: whatsapp_rust::Jid, quote: &Quote) -> Result<(), String> {
    let png = chart::render(quote, chrono::Local::now())?;
    let caption = chart::caption(quote);
    // A preview is a nicety, not the message: if it cannot be built the chart
    // still goes out without one.
    let thumbnail = chart::thumbnail(&png).ok();

    let upload = client
        .upload(png, MediaType::Image, UploadOptions::new())
        .await
        .map_err(|error| format!("Could not upload the chart: {error}"))?;

    // The upload carries PNG bytes, so the stanza has to say so: `ImageOptions`
    // would otherwise declare the default JPEG.
    let message = whatsapp_rust::media::image_message(
        upload,
        ImageOptions {
            caption: Some(caption),
            mimetype: Some("image/png".to_string()),
            jpeg_thumbnail: thumbnail,
            ..Default::default()
        },
    );

    client
        .send_message(jid, message)
        .await
        .map_err(|error| format!("Could not send the chart: {error}"))?;
    Ok(())
}
