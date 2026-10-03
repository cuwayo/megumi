//! Which groups want a price update, which symbols they watch, and which weeks
//! have already gone out.
//!
//! Like the news subscriptions, this lives in one JSON file rather than the
//! session database, so a setting an admin toggles survives a restart and a
//! week's chart is never posted twice. A chat's value is the list of symbols it
//! watches; presence of the key is the subscription, and a chat with no symbols
//! is never left behind — [`DEFAULT_SYMBOL`] seeds it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

/// The symbol a group watches when it subscribes without naming one: WTI crude,
/// the subject of the reference chart.
pub const DEFAULT_SYMBOL: &str = "CL=F";

/// The groups subscribed to the price update, their symbols, and the weeks sent.
pub struct PriceStore {
    path: PathBuf,
    state: Mutex<State>,
}

#[derive(Serialize, Deserialize, Default, Clone)]
struct State {
    /// Chat id → the symbols it watches. Presence of the key is the subscription.
    #[serde(default)]
    subscriptions: HashMap<String, Vec<String>>,
    /// Chat id → the weeks delivered to it, as the Monday's `YYYY-MM-DD`.
    #[serde(default)]
    deliveries: HashMap<String, Vec<String>>,
}

impl PriceStore {
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

    /// Whether `chat` gets the price update.
    pub fn is_enabled(&self, chat: &str) -> Result<bool, String> {
        Ok(self.state()?.subscriptions.contains_key(chat))
    }

    /// Subscribes or unsubscribes `chat`. Returns whether that changed anything.
    ///
    /// Subscribing with no symbols names [`DEFAULT_SYMBOL`], so a group that
    /// simply turns the update on has something to chart. The change is made on
    /// a copy and written before the in-memory state is replaced, so a write that
    /// fails leaves the store as it was and the caller can retry.
    pub fn set_enabled(&self, chat: &str, enabled: bool) -> Result<bool, String> {
        let mut state = self.state()?;
        let mut updated = state.clone();
        let changed = if enabled {
            if updated.subscriptions.contains_key(chat) {
                false
            } else {
                updated
                    .subscriptions
                    .insert(chat.to_string(), vec![DEFAULT_SYMBOL.to_string()]);
                true
            }
        } else {
            updated.subscriptions.remove(chat).is_some()
        };
        if changed {
            self.write(&updated)?;
            *state = updated;
        }
        Ok(changed)
    }

    /// The symbols `chat` watches, empty when it is not subscribed.
    pub fn symbols(&self, chat: &str) -> Result<Vec<String>, String> {
        Ok(self
            .state()?
            .subscriptions
            .get(chat)
            .cloned()
            .unwrap_or_default())
    }

    /// Adds `symbol` to `chat`'s watch list, subscribing it if it was not already.
    /// Returns whether that changed anything.
    pub fn add_symbol(&self, chat: &str, symbol: &str) -> Result<bool, String> {
        let symbol = symbol.trim().to_uppercase();
        if symbol.is_empty() {
            return Ok(false);
        }

        let mut state = self.state()?;
        let mut updated = state.clone();
        let symbols = updated.subscriptions.entry(chat.to_string()).or_default();
        let changed = if symbols.iter().any(|watched| watched == &symbol) {
            false
        } else {
            symbols.push(symbol);
            true
        };
        if changed {
            self.write(&updated)?;
            *state = updated;
        }
        Ok(changed)
    }

    /// Removes `symbol` from `chat`'s watch list, unsubscribing it when the list
    /// empties. Returns whether that changed anything.
    pub fn remove_symbol(&self, chat: &str, symbol: &str) -> Result<bool, String> {
        let symbol = symbol.trim().to_uppercase();

        let mut state = self.state()?;
        let mut updated = state.clone();
        let Some(symbols) = updated.subscriptions.get_mut(chat) else {
            return Ok(false);
        };
        let before = symbols.len();
        symbols.retain(|watched| watched != &symbol);
        if symbols.len() == before {
            return Ok(false);
        }
        if symbols.is_empty() {
            updated.subscriptions.remove(chat);
        }
        self.write(&updated)?;
        *state = updated;
        Ok(true)
    }

    /// Every chat currently subscribed, in no particular order.
    pub fn enabled_chats(&self) -> Result<Vec<String>, String> {
        Ok(self.state()?.subscriptions.keys().cloned().collect())
    }

    /// Whether the price update for `week` has already been delivered to `chat`.
    pub fn was_delivered(&self, chat: &str, week: NaiveDate) -> Result<bool, String> {
        let week = week.to_string();
        Ok(self
            .state()?
            .deliveries
            .get(chat)
            .is_some_and(|weeks| weeks.iter().any(|stored| stored == &week)))
    }

    /// Records that `week`'s update reached `chat`, so a restart will not resend it.
    ///
    /// As with [`set_enabled`](Self::set_enabled), the record is written before
    /// the in-memory state is replaced, so a failed write can be retried.
    pub fn mark_delivered(&self, chat: &str, week: NaiveDate) -> Result<(), String> {
        let mut state = self.state()?;
        let mut updated = state.clone();
        let week = week.to_string();
        let weeks = updated.deliveries.entry(chat.to_string()).or_default();
        if !weeks.contains(&week) {
            weeks.push(week);
            self.write(&updated)?;
            *state = updated;
        }
        Ok(())
    }

    fn state(&self) -> Result<std::sync::MutexGuard<'_, State>, String> {
        self.state
            .lock()
            .map_err(|_| "the price store lock was poisoned".to_string())
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
            .map_err(|error| format!("Failed to encode the price store: {error}"))?;
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

    fn store() -> PriceStore {
        PriceStore::open(":memory:").unwrap()
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
    fn subscribing_seeds_the_default_symbol() {
        let store = store();
        store.set_enabled("one@g.us", true).unwrap();
        assert_eq!(store.symbols("one@g.us").unwrap(), [DEFAULT_SYMBOL]);
    }

    #[test]
    fn symbols_are_added_case_insensitively_and_deduplicated() {
        let store = store();
        assert!(store.add_symbol("one@g.us", "gc=f").unwrap());
        // A second add of the same symbol changes nothing.
        assert!(!store.add_symbol("one@g.us", "GC=F").unwrap());
        assert_eq!(store.symbols("one@g.us").unwrap(), ["GC=F"]);
    }

    #[test]
    fn removing_the_last_symbol_unsubscribes_the_group() {
        let store = store();
        store.set_enabled("one@g.us", true).unwrap();
        assert!(store.remove_symbol("one@g.us", DEFAULT_SYMBOL).unwrap());
        assert!(!store.is_enabled("one@g.us").unwrap());
        // Removing again, or from an unknown chat, changes nothing.
        assert!(!store.remove_symbol("one@g.us", DEFAULT_SYMBOL).unwrap());
        assert!(!store.remove_symbol("two@g.us", "GC=F").unwrap());
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
    fn a_delivery_is_recorded_once_per_group_per_week() {
        let store = store();
        let monday = NaiveDate::from_ymd_opt(2026, 10, 5).unwrap();
        let next_monday = NaiveDate::from_ymd_opt(2026, 10, 12).unwrap();

        assert!(!store.was_delivered("one@g.us", monday).unwrap());
        store.mark_delivered("one@g.us", monday).unwrap();
        assert!(store.was_delivered("one@g.us", monday).unwrap());

        // A different week, and a different group, are each still pending.
        assert!(!store.was_delivered("one@g.us", next_monday).unwrap());
        assert!(!store.was_delivered("two@g.us", monday).unwrap());
    }

    #[test]
    fn the_file_survives_reopening() {
        let path = std::env::temp_dir().join(format!("megumi-price-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let monday = NaiveDate::from_ymd_opt(2026, 10, 5).unwrap();
        let store = PriceStore::open(&path).unwrap();
        store.set_enabled("one@g.us", true).unwrap();
        store.add_symbol("one@g.us", "GC=F").unwrap();
        store.mark_delivered("one@g.us", monday).unwrap();
        drop(store);

        let reopened = PriceStore::open(&path).unwrap();
        assert!(reopened.is_enabled("one@g.us").unwrap());
        assert_eq!(
            reopened.symbols("one@g.us").unwrap(),
            [DEFAULT_SYMBOL, "GC=F"]
        );
        assert!(reopened.was_delivered("one@g.us", monday).unwrap());
        let _ = std::fs::remove_file(&path);
    }
}
