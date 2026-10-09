//! The reminders a chat has set, and which have already fired.
//!
//! One JSON file for the whole bot, rewritten whole and atomically the way the
//! news store is: a reminder set before a restart is still there after one, and
//! a reminder that already fired is not sent a second time. `":memory:"` keeps
//! everything in the store and touches no disk, which is what the tests use.
//!
//! A reminder's fire time is stored as an absolute timestamp, not a delay, so a
//! restart reschedules it correctly. A reminder whose time passed while the bot
//! was offline is still returned by [`ReminderStore::due`], so it fires late
//! rather than being silently dropped.

use std::path::PathBuf;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use uuid::Uuid;

/// One reminder a chat asked for.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Reminder {
    /// A stable id, so `!remind cancel` can name one.
    pub id: String,
    /// The chat that set it.
    pub chat: String,
    /// When it should fire.
    pub due: DateTime<Utc>,
    /// What to say when it fires.
    pub text: String,
}

/// The reminders, one JSON file for the whole bot.
pub struct ReminderStore {
    path: PathBuf,
    state: Mutex<Vec<Reminder>>,
}

impl ReminderStore {
    /// Opens the store at `path`, reading what is already there.
    ///
    /// `":memory:"` keeps the state only in this store. A missing file is an
    /// empty store, created on the first write; an unreadable or unparseable
    /// file is an error, so a corrupt file fails at startup rather than on the
    /// next message.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, String> {
        let path = path.into();
        let state = if path.as_os_str() == ":memory:" || !path.exists() {
            Vec::new()
        } else {
            let text = std::fs::read_to_string(&path)
                .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
            serde_json::from_str(&text)
                .map_err(|error| format!("Failed to parse {}: {error}", path.display()))?
        };
        Ok(Self {
            path,
            state: Mutex::new(state),
        })
    }

    /// Adds a reminder for `chat` and returns it.
    pub fn add(&self, chat: &str, due: DateTime<Utc>, text: &str) -> Result<Reminder, String> {
        let reminder = Reminder {
            id: Uuid::new_v4().to_string(),
            chat: chat.to_string(),
            due,
            text: text.to_string(),
        };
        let mut state = self.state()?;
        let mut updated = state.clone();
        updated.push(reminder.clone());
        self.write(&updated)?;
        *state = updated;
        Ok(reminder)
    }

    /// The reminders `chat` has set, soonest first.
    pub fn list(&self, chat: &str) -> Result<Vec<Reminder>, String> {
        let mut reminders: Vec<Reminder> = self
            .state()?
            .iter()
            .filter(|reminder| reminder.chat == chat)
            .cloned()
            .collect();
        reminders.sort_by_key(|reminder| reminder.due);
        Ok(reminders)
    }

    /// Removes and returns the reminder of `chat` whose id starts with `prefix`.
    ///
    /// Scoped to `chat`, so a message can only cancel its own chat's reminders.
    /// An ambiguous prefix removes nothing and reports how many it matched.
    pub fn cancel(&self, chat: &str, prefix: &str) -> Result<CancelOutcome, String> {
        let mut state = self.state()?;
        let matches: Vec<String> = state
            .iter()
            .filter(|reminder| reminder.chat == chat && reminder.id.starts_with(prefix))
            .map(|reminder| reminder.id.clone())
            .collect();
        let target = match matches.len() {
            0 => return Ok(CancelOutcome::NoMatch),
            1 => matches.into_iter().next().expect("one match"),
            count => return Ok(CancelOutcome::Ambiguous { count }),
        };
        let mut updated = state.clone();
        updated.retain(|reminder| reminder.id != target);
        self.write(&updated)?;
        *state = updated;
        Ok(CancelOutcome::Cancelled)
    }

    /// Every reminder due at or before `now`, soonest first.
    ///
    /// A reminder overdue from before a restart is included, so it fires late.
    pub fn due(&self, now: DateTime<Utc>) -> Result<Vec<Reminder>, String> {
        let mut reminders: Vec<Reminder> = self
            .state()?
            .iter()
            .filter(|reminder| reminder.due <= now)
            .cloned()
            .collect();
        reminders.sort_by_key(|reminder| reminder.due);
        Ok(reminders)
    }

    /// Removes the reminder `id`, once it has been sent.
    ///
    /// Called after the send lands, so a crash between the send and this leaves
    /// the reminder in place and it is retried — at-least-once, never lost.
    pub fn mark_fired(&self, id: &str) -> Result<(), String> {
        let mut state = self.state()?;
        let mut updated = state.clone();
        let before = updated.len();
        updated.retain(|reminder| reminder.id != id);
        if updated.len() != before {
            self.write(&updated)?;
            *state = updated;
        }
        Ok(())
    }

    fn state(&self) -> Result<std::sync::MutexGuard<'_, Vec<Reminder>>, String> {
        self.state
            .lock()
            .map_err(|_| "the reminder store lock was poisoned".to_string())
    }

    /// Rewrites the file from scratch, atomically.
    fn write(&self, state: &[Reminder]) -> Result<(), String> {
        if self.path.as_os_str() == ":memory:" {
            return Ok(());
        }
        if let Some(parent) = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("Failed to create {}: {error}", parent.display()))?;
        }
        let text = serde_json::to_string_pretty(state)
            .map_err(|error| format!("Failed to encode the reminder store: {error}"))?;
        let temporary = self.path.with_extension("json.tmp");
        std::fs::write(&temporary, text)
            .map_err(|error| format!("Failed to write {}: {error}", temporary.display()))?;
        std::fs::rename(&temporary, &self.path).map_err(|error| {
            format!(
                "Failed to replace {} with {}: {error}",
                self.path.display(),
                temporary.display()
            )
        })
    }
}

/// What a [`ReminderStore::cancel`] call did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelOutcome {
    /// The reminder was removed.
    Cancelled,
    /// No reminder matched the id prefix.
    NoMatch,
    /// The prefix matched more than one reminder, so nothing was removed.
    Ambiguous {
        /// How many reminders the prefix matched.
        count: usize,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> ReminderStore {
        ReminderStore::open(":memory:").unwrap()
    }

    fn at(seconds: i64) -> DateTime<Utc> {
        Utc::now() + chrono::Duration::seconds(seconds)
    }

    #[test]
    fn a_reminder_is_listed_and_due_after_its_time() {
        let store = store();
        let reminder = store.add("gA", at(60), "stretch").unwrap();
        assert_eq!(store.list("gA").unwrap().len(), 1);
        assert!(store.due(Utc::now()).unwrap().is_empty());

        let due = store.due(at(61)).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, reminder.id);
    }

    #[test]
    fn an_overdue_reminder_still_fires() {
        let store = store();
        store.add("gA", at(-3_600), "late").unwrap();
        // Its time passed while the bot was offline; it fires rather than drops.
        assert_eq!(store.due(Utc::now()).unwrap().len(), 1);
    }

    #[test]
    fn list_and_cancel_are_scoped_to_the_chat() {
        let store = store();
        let mine = store.add("gA", at(60), "mine").unwrap();
        store.add("gB", at(60), "theirs").unwrap();

        assert_eq!(store.list("gA").unwrap().len(), 1);
        // Another chat cannot cancel it by prefix.
        assert_eq!(
            store.cancel("gB", &mine.id[..4]).unwrap(),
            CancelOutcome::NoMatch
        );
        assert_eq!(
            store.cancel("gA", &mine.id[..4]).unwrap(),
            CancelOutcome::Cancelled
        );
        assert!(store.list("gA").unwrap().is_empty());
    }

    #[test]
    fn an_ambiguous_cancel_removes_nothing() {
        let store = store();
        store.add("gA", at(60), "one").unwrap();
        store.add("gA", at(120), "two").unwrap();
        // The empty prefix matches both.
        assert_eq!(
            store.cancel("gA", "").unwrap(),
            CancelOutcome::Ambiguous { count: 2 }
        );
        assert_eq!(store.list("gA").unwrap().len(), 2);
    }

    #[test]
    fn marking_fired_removes_the_reminder() {
        let store = store();
        let reminder = store.add("gA", at(60), "x").unwrap();
        store.mark_fired(&reminder.id).unwrap();
        assert!(store.list("gA").unwrap().is_empty());
    }

    #[test]
    fn the_file_survives_reopening() {
        let path = std::env::temp_dir().join(format!("megumi-remind-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let store = ReminderStore::open(&path).unwrap();
        let reminder = store.add("gA", at(3_600), "stretch").unwrap();
        drop(store);

        let reopened = ReminderStore::open(&path).unwrap();
        let listed = reopened.list("gA").unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, reminder.id);
        assert_eq!(listed[0].text, "stretch");
        let _ = std::fs::remove_file(&path);
    }
}
