//! Reading a reminder's delay out of what a user typed.
//!
//! `!remind set 1h30m stand up` sets a reminder an hour and a half out. The
//! grammar is a sequence of `<number><unit>` pairs with no spaces, where the
//! unit is one of `s`, `m`, `h`, or `d`; a bare number is read as minutes. This
//! is a pure function of the text, so it is tested without a clock.

use std::time::Duration;

/// The most a reminder may be set for, so a typo like `9999d` is refused rather
/// than stored.
const MAX: Duration = Duration::from_secs(365 * 24 * 60 * 60);

/// Parses `text` as a delay, or `None` when it is not one.
///
/// Accepts `90s`, `10m`, `1h30m`, `2d`, and a bare number as minutes (`30`).
/// The parts may be in any order and are summed; each number must be a positive
/// integer. A zero total, an unknown unit, or trailing junk is rejected.
pub fn parse_duration(text: &str) -> Option<Duration> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    // A bare number is minutes, the unit a person most often omits.
    if let Ok(minutes) = text.parse::<u64>() {
        return minutes
            .checked_mul(60)
            .map(Duration::from_secs)
            .filter(nonzero);
    }

    let mut total = Duration::ZERO;
    let mut number = String::new();
    let mut saw_part = false;
    for ch in text.chars() {
        if ch.is_ascii_digit() {
            number.push(ch);
            continue;
        }
        let unit = match ch {
            's' => 1,
            'm' => 60,
            'h' => 3_600,
            'd' => 86_400,
            _ => return None,
        };
        let value: u64 = number.parse().ok()?;
        number.clear();
        saw_part = true;
        total = total.checked_add(Duration::from_secs(value.checked_mul(unit)?))?;
    }
    // A trailing number with no unit is junk ("1h30").
    if !number.is_empty() || !saw_part {
        return None;
    }
    (total <= MAX).then_some(total).filter(nonzero)
}

/// Whether a duration is not zero.
fn nonzero(duration: &Duration) -> bool {
    !duration.is_zero()
}

/// `duration` as a short human phrase, for the confirmation reply.
pub fn humanize(duration: Duration) -> String {
    let total = duration.as_secs();
    let days = total / 86_400;
    let hours = (total % 86_400) / 3_600;
    let minutes = (total % 3_600) / 60;
    let seconds = total % 60;

    let mut parts = Vec::new();
    for (count, name) in [
        (days, "day"),
        (hours, "hour"),
        (minutes, "minute"),
        (seconds, "second"),
    ] {
        if count > 0 {
            parts.push(if count == 1 {
                format!("{count} {name}")
            } else {
                format!("{count} {name}s")
            });
        }
    }
    if parts.is_empty() {
        return "0 seconds".to_string();
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_units_parse() {
        assert_eq!(parse_duration("90s"), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration("10m"), Some(Duration::from_secs(600)));
        assert_eq!(parse_duration("2h"), Some(Duration::from_secs(7_200)));
        assert_eq!(parse_duration("1d"), Some(Duration::from_secs(86_400)));
    }

    #[test]
    fn parts_are_summed() {
        assert_eq!(parse_duration("1h30m"), Some(Duration::from_secs(5_400)));
        assert_eq!(
            parse_duration("1d2h3m4s"),
            Some(Duration::from_secs(86_400 + 7_200 + 180 + 4))
        );
    }

    #[test]
    fn a_bare_number_is_minutes() {
        assert_eq!(parse_duration("30"), Some(Duration::from_secs(1_800)));
    }

    #[test]
    fn junk_is_rejected() {
        for text in [
            "", "soon", "1x", "1h30", "h", "0", "0m", "-5m", "1 h", "9999d",
        ] {
            assert_eq!(parse_duration(text), None, "{text:?}");
        }
    }

    #[test]
    fn humanize_reads_naturally() {
        assert_eq!(humanize(Duration::from_secs(1)), "1 second");
        assert_eq!(humanize(Duration::from_secs(5_400)), "1 hour 30 minutes");
        assert_eq!(humanize(Duration::from_secs(86_400)), "1 day");
        assert_eq!(humanize(Duration::ZERO), "0 seconds");
    }
}
