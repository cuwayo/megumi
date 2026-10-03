//! The commands this bot serves, and the framework they are registered with.
//!
//! `main` wires this registry into the WhatsApp client; the integration tests
//! under `tests/` drive the same registry.

use std::sync::Arc;
use std::time::Instant;

use megumi::Framework;

pub mod commands;
mod data;
pub mod news;
pub mod price;

pub use data::Data;
pub use news::store::NewsStore;
pub use price::store::PriceStore;

/// The context every command takes: the framework's [`Context`](megumi::Context)
/// carrying the bot's [`Data`].
pub type Context = megumi::Context<Data>;

/// The bot's command registry: the prefix it answers to, and every group of
/// commands it serves.
///
/// Commands are declared in groups (see [`commands`]), which is what the help
/// listing groups them by. `Data::started` is pinned here, and `main` builds the
/// framework before anything else, so `!uptime` is process lifetime rather than
/// time since the first `!uptime`.
pub fn framework() -> Framework<Data> {
    // `NEWS_DB` and `PRICE_DB` override the defaults beside the session file.
    // Tests build a framework per assertion and never send a digest or a chart, so
    // they get in-memory databases instead of ones on disk.
    let news_path = std::env::var("NEWS_DB").unwrap_or_else(|_| {
        if cfg!(test) {
            ":memory:".to_string()
        } else {
            "news.json".to_string()
        }
    });
    let price_path = std::env::var("PRICE_DB").unwrap_or_else(|_| {
        if cfg!(test) {
            ":memory:".to_string()
        } else {
            "price.json".to_string()
        }
    });

    Framework::builder()
        .setup(|| Data {
            started: Instant::now(),
            news: Arc::new(NewsStore::open(news_path).unwrap_or_else(|error| panic!("{error}"))),
            price: Arc::new(PriceStore::open(price_path).unwrap_or_else(|error| panic!("{error}"))),
        })
        .prefix("!")
        .groups([
            commands::utility(),
            commands::media(),
            commands::admin(),
            commands::owner(),
        ])
        .build()
}
