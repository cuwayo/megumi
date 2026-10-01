use std::time::Instant;

/// The state every command shares, built once in [`crate::framework`].
///
/// The start time is pinned here rather than in a process-global static so
/// `!uptime` reads the same value the framework was built with, and a test
/// that builds its own framework starts its own clock.
pub struct Data {
    /// When this framework was built, which is when the process started: `main`
    /// builds the framework before anything else.
    pub started: Instant,
}
