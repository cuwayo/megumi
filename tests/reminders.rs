//! The reminder store and its duration parser, through the public API.
//!
//! The scheduler loop needs a live WhatsApp client, so it is not exercised
//! here; these cover the two pure pieces it is built on — reading a delay out of
//! what a user typed, and the store that survives a restart.

use std::time::Duration;

use megumi_whatsapp::ReminderStore;
use megumi_whatsapp::reminders::{humanize, parse_duration};

#[test]
fn durations_parse_from_what_a_user_types() {
    assert_eq!(parse_duration("90s"), Some(Duration::from_secs(90)));
    assert_eq!(parse_duration("10m"), Some(Duration::from_secs(600)));
    assert_eq!(parse_duration("1h30m"), Some(Duration::from_secs(5_400)));
    assert_eq!(parse_duration("2d"), Some(Duration::from_secs(172_800)));
    // A bare number is minutes.
    assert_eq!(parse_duration("30"), Some(Duration::from_secs(1_800)));
    // Junk is refused rather than guessed at.
    assert_eq!(parse_duration("soon"), None);
    assert_eq!(parse_duration("1h30"), None);
}

#[test]
fn a_reminder_survives_a_restart() {
    let path = std::env::temp_dir().join(format!("megumi-remind-it-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&path);

    let store = ReminderStore::open(&path).unwrap();
    let due = chrono::Utc::now() + chrono::Duration::seconds(3_600);
    let reminder = store.add("120363@g.us", due, "take a break").unwrap();
    drop(store);

    let reopened = ReminderStore::open(&path).unwrap();
    let listed = reopened.list("120363@g.us").unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, reminder.id);
    assert_eq!(listed[0].text, "take a break");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_reminder_is_scoped_to_its_chat() {
    let store = ReminderStore::open(":memory:").unwrap();
    let due = chrono::Utc::now() + chrono::Duration::seconds(60);
    store.add("gA", due, "mine").unwrap();

    assert!(store.list("gB").unwrap().is_empty());
    // Another chat cannot cancel it.
    let mine = &store.list("gA").unwrap()[0].id;
    assert!(matches!(
        store.cancel("gB", &mine[..4]).unwrap(),
        megumi_whatsapp::reminders::store::CancelOutcome::NoMatch
    ));
}

#[test]
fn humanize_reads_naturally() {
    assert_eq!(humanize(Duration::from_secs(5_400)), "1 hour 30 minutes");
    assert_eq!(humanize(Duration::from_secs(86_400)), "1 day");
}
