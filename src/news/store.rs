//! Which groups want a morning digest, and which mornings have already gone out.
//!
//! Both live in one JSON file rather than the session database, so a setting an
//! admin toggles is still there after a restart and a digest is never posted
//! twice for the same morning. The file is rewritten whole: it holds one line
//! per subscribed group, and nothing here is worth a database.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

/// The groups subscribed to the morning digest, and the mornings already sent.
pub struct NewsStore {
    path: PathBuf,
    state: Mutex<State>,
}

#[derive(Serialize, Deserialize, Default, Clone)]
struct State {
    /// Chat id → nothing. Presence is the whole subscription.
    #[serde(default)]
    subscriptions: HashMap<String, ()>,
    /// Chat id → the mornings delivered to it, as `YYYY-MM-DD`.
    #[serde(default)]
    deliveries: HashMap<String, Vec<String>>,
}

impl NewsStore {
    /// Opens the store at `path`, reading what is already there.
    ///
    /// A missing file is an empty store, created on the first change. `":memory:"`
    /// keeps the state only in this store, which is what the tests use.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, String> {
        let path = path.into();
        let state = if path.as_os_str() == ":memory:" || !path.exists() {
            State::default()
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

    /// Whether `chat` gets the morning digest.
    pub fn is_enabled(&self, chat: &str) -> Result<bool, String> {
        Ok(self.state()?.subscriptions.contains_key(chat))
    }

    /// Subscribes or unsubscribes `chat`. Returns whether that changed anything.
    ///
    /// The change is made on a copy and written before the in-memory state is
    /// replaced, so a write that fails leaves the store as it was and the caller
    /// can retry.
    pub fn set_enabled(&self, chat: &str, enabled: bool) -> Result<bool, String> {
        let mut state = self.state()?;
        let mut updated = state.clone();
        let changed = if enabled {
            updated.subscriptions.insert(chat.to_string(), ()).is_none()
        } else {
            updated.subscriptions.remove(chat).is_some()
        };
        if changed {
            self.write(&updated)?;
            *state = updated;
        }
        Ok(changed)
    }

    /// Every chat currently subscribed, in no particular order.
    pub fn enabled_chats(&self) -> Result<Vec<String>, String> {
        Ok(self.state()?.subscriptions.keys().cloned().collect())
    }

    /// Whether the digest for `day` has already been delivered to `chat`.
    pub fn was_delivered(&self, chat: &str, day: NaiveDate) -> Result<bool, String> {
        let day = day.to_string();
        Ok(self
            .state()?
            .deliveries
            .get(chat)
            .is_some_and(|days| days.iter().any(|stored| stored == &day)))
    }

    /// Records that `day`'s digest reached `chat`, so a restart will not resend it.
    ///
    /// As with [`set_enabled`](Self::set_enabled), the record is written before
    /// the in-memory state is replaced, so a failed write can be retried.
    pub fn mark_delivered(&self, chat: &str, day: NaiveDate) -> Result<(), String> {
        let mut state = self.state()?;
        let mut updated = state.clone();
        let day = day.to_string();
        let days = updated.deliveries.entry(chat.to_string()).or_default();
        if !days.contains(&day) {
            days.push(day);
            self.write(&updated)?;
            *state = updated;
        }
        Ok(())
    }

    fn state(&self) -> Result<std::sync::MutexGuard<'_, State>, String> {
        self.state
            .lock()
            .map_err(|_| "the news store lock was poisoned".to_string())
    }

    /// Rewrites the file from scratch. The temporary name sits beside it, so the
    /// rename that replaces the real file is atomic on the same filesystem.
    fn write(&self, state: &State) -> Result<(), String> {
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
            .map_err(|error| format!("Failed to encode the news store: {error}"))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> NewsStore {
        NewsStore::open(":memory:").unwrap()
    }

    #[test]
    fn a_group_starts_unsubscribed_and_toggles() {
        let store = store();
        assert!(!store.is_enabled("120363@g.us").unwrap());

        assert!(store.set_enabled("120363@g.us", true).unwrap());
        // Enabling twice changes nothing, which is what the reply distinguishes.
        assert!(!store.set_enabled("120363@g.us", true).unwrap());
        assert!(store.is_enabled("120363@g.us").unwrap());

        assert!(store.set_enabled("120363@g.us", false).unwrap());
        assert!(!store.is_enabled("120363@g.us").unwrap());
        assert!(!store.set_enabled("120363@g.us", false).unwrap());
    }

    #[test]
    fn enabled_chats_lists_only_subscribers() {
        let store = store();
        store.set_enabled("one@g.us", true).unwrap();
        store.set_enabled("two@g.us", true).unwrap();
        store.set_enabled("three@g.us", true).unwrap();
        store.set_enabled("three@g.us", false).unwrap();

        let mut chats = store.enabled_chats().unwrap();
        chats.sort();
        assert_eq!(chats, ["one@g.us", "two@g.us"]);
    }

    #[test]
    fn a_delivery_is_recorded_once_per_group_per_day() {
        let store = store();
        let monday = NaiveDate::from_ymd_opt(2026, 10, 5).unwrap();
        let tuesday = NaiveDate::from_ymd_opt(2026, 10, 6).unwrap();

        assert!(!store.was_delivered("one@g.us", monday).unwrap());
        store.mark_delivered("one@g.us", monday).unwrap();
        assert!(store.was_delivered("one@g.us", monday).unwrap());

        // A different morning, and a different group, are each still pending.
        assert!(!store.was_delivered("one@g.us", tuesday).unwrap());
        assert!(!store.was_delivered("two@g.us", monday).unwrap());
    }

    #[test]
    fn the_file_survives_reopening() {
        let path = std::env::temp_dir().join(format!("megumi-news-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let monday = NaiveDate::from_ymd_opt(2026, 10, 5).unwrap();
        let store = NewsStore::open(&path).unwrap();
        store.set_enabled("one@g.us", true).unwrap();
        store.mark_delivered("one@g.us", monday).unwrap();
        drop(store);

        let reopened = NewsStore::open(&path).unwrap();
        assert!(reopened.is_enabled("one@g.us").unwrap());
        assert!(reopened.was_delivered("one@g.us", monday).unwrap());
        let _ = std::fs::remove_file(&path);
    }
}
