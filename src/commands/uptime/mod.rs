use std::time::Duration;

use megumi::{Error, command};

use crate::Context;

/// Reports how long the bot has been running.
#[command(name = "uptime")]
async fn uptime(ctx: Context) -> Result<(), Error> {
    ctx.say(format_uptime(ctx.data().started.elapsed())).await
}

/// `uptime` as a human reads it: only the units that are present, largest
/// first, so a freshly started bot says "0 seconds" and a long run does not
/// pad out with "0 days".
fn format_uptime(elapsed: Duration) -> String {
    let total = elapsed.as_secs();
    let days = total / 86_400;
    let hours = (total % 86_400) / 3_600;
    let minutes = (total % 3_600) / 60;
    let seconds = total % 60;

    let mut parts = Vec::new();
    if days > 0 {
        parts.push(unit(days, "day"));
    }
    if hours > 0 {
        parts.push(unit(hours, "hour"));
    }
    if minutes > 0 {
        parts.push(unit(minutes, "minute"));
    }
    if seconds > 0 || parts.is_empty() {
        parts.push(unit(seconds, "second"));
    }
    parts.join(", ")
}

fn unit(count: u64, name: &str) -> String {
    if count == 1 {
        format!("{count} {name}")
    } else {
        format!("{count} {name}s")
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::format_uptime;

    #[test]
    fn zero_is_zero_seconds() {
        assert_eq!(format_uptime(Duration::ZERO), "0 seconds");
    }

    #[test]
    fn a_single_unit_is_not_pluralised() {
        assert_eq!(format_uptime(Duration::from_secs(1)), "1 second");
        assert_eq!(format_uptime(Duration::from_secs(60)), "1 minute");
        assert_eq!(format_uptime(Duration::from_secs(3_600)), "1 hour");
        assert_eq!(format_uptime(Duration::from_secs(86_400)), "1 day");
    }

    #[test]
    fn absent_units_are_dropped() {
        assert_eq!(
            format_uptime(Duration::from_secs(90)),
            "1 minute, 30 seconds"
        );
        assert_eq!(
            format_uptime(Duration::from_secs(86_400 + 45)),
            "1 day, 45 seconds"
        );
        assert_eq!(
            format_uptime(Duration::from_secs(2 * 86_400 + 3 * 3_600 + 4 * 60 + 5)),
            "2 days, 3 hours, 4 minutes, 5 seconds"
        );
    }

    #[test]
    fn sub_second_uptime_reads_as_zero_seconds() {
        assert_eq!(format_uptime(Duration::from_millis(999)), "0 seconds");
    }
}
