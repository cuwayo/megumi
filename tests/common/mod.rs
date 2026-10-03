//! Shared setup for the integration tests.
//!
//! These tests link the bot as an external crate, so the library's own
//! `cfg!(test)` is not set inside it and [`framework`](megumi_whatsapp::framework)
//! would open the real `news.json` and `price.json` rather than in-memory stores.
//! Point them at `:memory:` once per test binary, before the first framework is
//! built. The tests only inspect the registry and never send a digest or a chart,
//! so the stores' contents are irrelevant either way.

use std::sync::Once;

use megumi::Framework;
use megumi_whatsapp::{Data, framework as build};

/// Builds the bot's command registry against in-memory stores.
pub fn framework() -> Framework<Data> {
    static SET_STORE_PATHS: Once = Once::new();
    SET_STORE_PATHS.call_once(|| {
        // SAFETY: this binary touches these variables only in this write and the
        // reads `build` does below. `call_once` synchronises, so every caller's
        // read follows the write, and no read runs concurrently with it.
        unsafe {
            std::env::set_var("NEWS_DB", ":memory:");
            std::env::set_var("PRICE_DB", ":memory:");
        }
    });

    build()
}
