//! When the weekly price update counts as due.
//!
//! Like the news digest, the update goes out at a fixed hour in the machine's
//! local time, but only on one weekday. "Has this week's hour arrived" is a pure
//! function of a timestamp, so the decision can be tested without waiting for a
//! clock. The week it returns is the Monday of that week, which is what the
//! delivery record is keyed by — so the weekday can be moved at any time without
//! ever posting a second chart into a week already served.

use chrono::{DateTime, Datelike, Local, NaiveDate};

/// The local hour the update goes out at, unless `PRICE_HOUR` says otherwise.
pub const DEFAULT_HOUR: u32 = 9;

/// The local weekday the update goes out on, unless `PRICE_WEEKDAY` says
/// otherwise. Monday, so the week opens with the chart.
pub const DEFAULT_WEEKDAY: chrono::Weekday = chrono::Weekday::Mon;

/// The local hour the update goes out at.
///
/// `PRICE_HOUR=17` moves it to five in the afternoon. An unset, empty, or
/// unusable value keeps [`DEFAULT_HOUR`], because a misconfigured hour should
/// fall back to the ordinary time rather than silently disable the feature.
pub fn price_hour() -> u32 {
    std::env::var("PRICE_HOUR")
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .filter(|hour| *hour < 24)
        .unwrap_or(DEFAULT_HOUR)
}

/// The local weekday the update goes out on.
///
/// `PRICE_WEEKDAY=friday` moves it to Friday. The name is matched
/// case-insensitively against a full or three-letter weekday; an unset, empty, or
/// unrecognised value keeps [`DEFAULT_WEEKDAY`], for the same reason a bad hour
/// does.
pub fn price_weekday() -> chrono::Weekday {
    std::env::var("PRICE_WEEKDAY")
        .ok()
        .and_then(|value| parse_weekday(value.trim()))
        .unwrap_or(DEFAULT_WEEKDAY)
}

/// A weekday from a name like `Monday` or `mon`, matched on its first three
/// letters so either form works. Anything shorter or unknown is `None`.
fn parse_weekday(value: &str) -> Option<chrono::Weekday> {
    const NAMES: [(&str, chrono::Weekday); 7] = [
        ("mon", chrono::Weekday::Mon),
        ("tue", chrono::Weekday::Tue),
        ("wed", chrono::Weekday::Wed),
        ("thu", chrono::Weekday::Thu),
        ("fri", chrono::Weekday::Fri),
        ("sat", chrono::Weekday::Sat),
        ("sun", chrono::Weekday::Sun),
    ];
    let value = value.to_ascii_lowercase();
    NAMES
        .iter()
        .find(|(name, _)| value.starts_with(name))
        .map(|(_, weekday)| *weekday)
}

/// The Monday of the week `now` belongs to, once this week's hour has arrived.
///
/// Before that hour it is still last week, which has already gone out, so the
/// answer is `None`. At and after the hour it is the current week, which is what
/// the delivery record is keyed by.
pub fn window_of(now: DateTime<Local>) -> Option<NaiveDate> {
    window_of_at(now, price_weekday(), price_hour())
}

/// [`window_of`] against an explicit weekday and hour, so the boundary can be
/// tested without touching `PRICE_WEEKDAY` or `PRICE_HOUR` in the process
/// environment.
fn window_of_at(now: DateTime<Local>, weekday: chrono::Weekday, hour: u32) -> Option<NaiveDate> {
    // The hour comes out of the formatted time: the `clock` feature that adds
    // `NaiveTime::hour` is not in the resolved chrono, and formatting needs nothing.
    let current: u32 = now.format("%H").to_string().parse().unwrap_or(0);
    let today = now.weekday();
    (today == weekday && current >= hour)
        .then(|| now.date_naive() - chrono::Duration::days(today.num_days_from_monday() as i64))
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    /// Monday, 5 October 2026, at `hour:minute`.
    fn at(hour: u32, minute: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 10, 5, hour, minute, 0)
            .single()
            .expect("the timestamp should be valid")
    }

    #[test]
    fn the_update_is_due_on_the_configured_weekday_and_hour() {
        let monday = chrono::Weekday::Mon;
        assert_eq!(
            window_of_at(at(8, 59), monday, DEFAULT_HOUR),
            None,
            "one minute early is too soon"
        );
        assert_eq!(
            window_of_at(at(DEFAULT_HOUR, 0), monday, DEFAULT_HOUR).map(|day| day.to_string()),
            Some("2026-10-05".to_string()),
            "the hour itself counts"
        );
        assert!(
            window_of_at(at(23, 30), monday, DEFAULT_HOUR).is_some(),
            "the rest of the day counts"
        );
    }

    #[test]
    fn another_weekday_is_never_due() {
        assert_eq!(
            window_of_at(at(DEFAULT_HOUR, 0), chrono::Weekday::Fri, DEFAULT_HOUR),
            None,
            "the week has not reached the configured day"
        );
    }

    #[test]
    fn the_key_is_the_monday_of_the_week() {
        // Friday 9 October 2026 is the same week as Monday 5 October.
        let friday = Local
            .with_ymd_and_hms(2026, 10, 9, DEFAULT_HOUR, 0, 0)
            .single()
            .unwrap();
        assert_eq!(
            window_of_at(friday, chrono::Weekday::Fri, DEFAULT_HOUR).map(|day| day.to_string()),
            Some("2026-10-05".to_string()),
            "a Friday send records the week's Monday"
        );
    }
}
