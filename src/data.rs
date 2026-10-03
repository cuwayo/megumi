use std::sync::Arc;
use std::time::Instant;

use crate::news::store::NewsStore;
use crate::price::store::PriceStore;

/// The state every command shares, built once in [`crate::framework`].
///
/// The start time is pinned here rather than in a process-global static so
/// `!uptime` reads the same value the framework was built with, and a test
/// that builds its own framework starts its own clock.
#[derive(Clone)]
pub struct Data {
    /// When this framework was built, which is when the process started: `main`
    /// builds the framework before anything else.
    pub started: Instant,
    /// Which groups receive the morning digest. Shared with the scheduler, which
    /// holds the same `Data` the commands do.
    pub news: Arc<NewsStore>,
    /// Which groups receive the weekly price update, and which symbols they watch.
    /// Shared with the price scheduler the same way `news` is.
    pub price: Arc<PriceStore>,
}
