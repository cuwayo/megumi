use std::sync::Arc;
use std::time::Instant;

use crate::news::store::NewsStore;

/// The state every command shares, built once by the framework's async `setup`.
///
/// The start time is pinned here rather than in a process-global static so
/// `!uptime` reads the same value the framework was built with, and a test
/// that builds its own framework starts its own clock. `setup` captures the
/// instant at build time, so `started` still measures the process lifetime
/// even though the value is not produced until the first event arrives.
pub struct Data {
    /// When the framework was built, which is when the process started: `main`
    /// builds the framework before anything else.
    pub started: Instant,
    /// Which groups receive the morning digest. Shared with the scheduler, which
    /// holds the same `NewsStore` the commands reach through `ctx.data().news`.
    pub news: Arc<NewsStore>,
    /// The digest loop the current connection started, so a reconnect can stop
    /// it before starting its replacement. `Event::Connected` fires again on
    /// every reconnect, and the loop runs forever, so without this each
    /// reconnect would leave another loop running beside the new one.
    pub news_task: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}
