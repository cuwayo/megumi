//! Shared setup for the integration tests.
//!
//! These tests link the bot as an external crate, so the library's own
//! `cfg!(test)` is not set inside it and [`framework`](megumi_whatsapp::framework)
//! would open the real `news.json` rather than the in-memory store. Point it at
//! `:memory:` once per test binary, before the first framework is built. The
//! tests only inspect the registry and never send a digest, so the store's
//! contents are irrelevant either way.

use std::sync::Once;

use megumi::Framework;
use megumi_whatsapp::{Data, framework as build};

/// Builds the bot's command registry against an in-memory news store.
pub fn framework() -> Framework<Data> {
    static SET_NEWS_DB: Once = Once::new();
    SET_NEWS_DB.call_once(|| {
        // SAFETY: this binary touches `NEWS_DB` only in this write and the read
        // `build` does below. `call_once` synchronises, so every caller's read
        // follows the write, and no read runs concurrently with it.
        unsafe { std::env::set_var("NEWS_DB", ":memory:") };
    });

    build()
}
