//! When a morning counts as started.
//!
//! The digest goes out at [`DIGEST_HOUR`] in the machine's local time. "Has this
//! morning started" is a pure function of a timestamp, so the decision can be
//! tested without waiting for a clock.

use chrono::{DateTime, Local, NaiveDate};

/// The local hour the digest goes out at, unless `NEWS_HOUR` says otherwise.
pub const DEFAULT_HOUR: u32 = 7;

/// The local hour the digest goes out at.
///
/// `NEWS_HOUR=5` moves it to five in the morning. An unset, empty, or unusable
/// value keeps [`DEFAULT_HOUR`], because a misconfigured hour should fall back to
/// the ordinary morning rather than silently disable the feature.
pub fn digest_hour() -> u32 {
    std::env::var("NEWS_HOUR")
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .filter(|hour| *hour < 24)
        .unwrap_or(DEFAULT_HOUR)
}

/// The morning `now` belongs to, once [`digest_hour`] has been reached today.
///
/// Before that hour it is still yesterday's morning, which has already gone out,
/// so the answer is `None`. At and after the hour it is today's date, which is
/// what the delivery record is keyed by.
pub fn morning_of(now: DateTime<Local>) -> Option<NaiveDate> {
    morning_of_at(now, digest_hour())
}

/// [`morning_of`] against an explicit hour, so the boundary can be tested without
/// touching `NEWS_HOUR` in the process environment.
fn morning_of_at(now: DateTime<Local>, hour: u32) -> Option<NaiveDate> {
    // The hour comes out of the formatted time: the `clock` feature that adds
    // `NaiveTime::hour` is not in the resolved chrono, and formatting needs nothing.
    let current: u32 = now.format("%H").to_string().parse().unwrap_or(0);
    (current >= hour).then(|| now.date_naive())
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn at(hour: u32, minute: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 10, 5, hour, minute, 0)
            .single()
            .expect("the timestamp should be valid")
    }

    #[test]
    fn the_morning_starts_at_the_configured_hour() {
        assert_eq!(
            morning_of_at(at(6, 59), DEFAULT_HOUR),
            None,
            "one minute early is too soon"
        );
        assert_eq!(
            morning_of_at(at(DEFAULT_HOUR, 0), DEFAULT_HOUR).map(|day| day.to_string()),
            Some("2026-10-05".to_string()),
            "the hour itself counts"
        );
        assert!(
            morning_of_at(at(23, 30), DEFAULT_HOUR).is_some(),
            "the rest of the day counts"
        );
    }
}
