//! How long to wait before fetching the digest again after a failure.
//!
//! The scheduler wakes every minute, and a morning that cannot be fetched stays
//! pending for the rest of the day, so without a backoff an unreachable feed
//! would be asked once a minute until midnight. The wait doubles with each
//! consecutive failure and is capped, and a fetch that works — or a new
//! morning — starts the count over. The decision is a pure function of the
//! clock handed in, so it can be tested without waiting.

use std::time::{Duration, Instant};

use chrono::NaiveDate;

/// The wait after the first failed fetch. Each further failure doubles it.
const RETRY_BASE: Duration = Duration::from_secs(5 * 60);

/// The longest wait between attempts, so a day-long outage still retries.
const RETRY_MAX: Duration = Duration::from_secs(60 * 60);

/// The record of failed digest fetches for one morning.
#[derive(Default)]
pub struct Backoff {
    /// The morning the failures below belong to. A different day starts fresh.
    day: Option<NaiveDate>,
    /// How many fetches have failed in a row for `day`.
    failures: u32,
    /// The earliest time the next attempt may be made, absent any failure.
    next_attempt: Option<Instant>,
}

impl Backoff {
    /// Whether a fetch for `day` may be attempted at `now`.
    pub fn ready(&self, day: NaiveDate, now: Instant) -> bool {
        // A new morning, or a run with no failures behind it, always fetches.
        self.day != Some(day) || self.next_attempt.is_none_or(|at| now >= at)
    }

    /// Records that the fetch for `day` failed at `now`, pushing the next
    /// attempt out by an exponentially longer wait.
    pub fn record_failure(&mut self, day: NaiveDate, now: Instant) {
        if self.day != Some(day) {
            self.day = Some(day);
            self.failures = 0;
        }
        self.failures += 1;

        // The first failure waits RETRY_BASE, then the wait doubles. The shift
        // is capped well below the point where RETRY_MAX already applies, so a
        // long outage cannot overflow it.
        let factor = 1u32 << self.failures.saturating_sub(1).min(16);
        let delay = RETRY_BASE.saturating_mul(factor).min(RETRY_MAX);
        self.next_attempt = Some(now + delay);
    }

    /// Clears the record, so the next fetch is attempted immediately.
    pub fn record_success(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 5).expect("the date should be valid")
    }

    #[test]
    fn a_fresh_backoff_fetches_at_once() {
        let backoff = Backoff::default();
        assert!(backoff.ready(day(), Instant::now()));
    }

    #[test]
    fn the_wait_doubles_from_the_base_and_stops_at_the_cap() {
        let mut backoff = Backoff::default();
        let mut now = Instant::now();

        for expected in [
            RETRY_BASE,
            RETRY_BASE * 2,
            RETRY_BASE * 4,
            RETRY_BASE * 8,
            RETRY_MAX,
            RETRY_MAX,
        ] {
            backoff.record_failure(day(), now);
            assert!(
                !backoff.ready(day(), now + expected - Duration::from_secs(1)),
                "not ready a second early"
            );
            assert!(
                backoff.ready(day(), now + expected),
                "ready after {expected:?}"
            );
            now += expected;
        }
    }

    #[test]
    fn a_successful_fetch_clears_the_record() {
        let mut backoff = Backoff::default();
        let base = Instant::now();

        backoff.record_failure(day(), base);
        assert!(!backoff.ready(day(), base), "held back right after failing");

        backoff.record_success();
        assert!(backoff.ready(day(), base), "ready again right away");
    }

    #[test]
    fn a_new_morning_starts_over() {
        let first = NaiveDate::from_ymd_opt(2026, 10, 5).unwrap();
        let second = NaiveDate::from_ymd_opt(2026, 10, 6).unwrap();
        let base = Instant::now();
        let mut backoff = Backoff::default();

        backoff.record_failure(first, base);
        assert!(backoff.ready(second, base), "a new day is not held back");

        // The failure count is forgotten too, so the wait is the base again.
        backoff.record_failure(second, base);
        assert!(!backoff.ready(second, base + RETRY_BASE - Duration::from_secs(1)));
        assert!(backoff.ready(second, base + RETRY_BASE));
    }
}
