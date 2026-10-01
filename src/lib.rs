//! The commands this bot serves, and the framework they are registered with.
//!
//! `main` wires this registry into the WhatsApp client; the integration tests
//! under `tests/` drive the same registry.

use std::time::Instant;

use megumi::Framework;

pub mod commands;
mod data;

pub use data::Data;

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
    Framework::builder()
        .setup(|| Data {
            started: Instant::now(),
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
