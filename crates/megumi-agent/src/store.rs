//! The message store: what every chat has said, and its rolling summary.
//!
//! The agent stores every message it sees, in every chat, whether or not it
//! answers — that history is what the context builder and, later, memory
//! extraction read. Storage is one JSON file per chat, so a chat that grows
//! does not force a rewrite of every other chat, and each file holds a bounded
//! window rather than an ever-growing transcript. Writes follow the same
//! atomic-rewrite pattern as the bot's news store: serialise, write a temporary
//! file beside the target, then rename over it, so a crash mid-write cannot
//! leave a half-written file in place.
//!
//! `":memory:"` keeps everything in the store and touches no disk, which is what
//! the tests use.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::event::{ChatId, SenderId};

/// One stored message, as the context builder and summarizer read it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredMessage {
    /// The platform's message id, used for idempotent appends.
    pub message_id: String,
    /// The author's stable identity.
    pub sender: SenderId,
    /// The author's display name, when known.
    pub sender_name: Option<String>,
    /// The message text, when it had any.
    pub text: Option<String>,
    /// Whether the agent itself sent this message.
    pub from_self: bool,
    /// When the message was sent.
    pub timestamp: DateTime<Utc>,
}

/// Everything the store keeps for one chat.
#[derive(Serialize, Deserialize, Clone)]
struct ChatState {
    /// The chat this file belongs to, so a file can be read back without
    /// decoding its name.
    chat: ChatId,
    /// The most recent messages, oldest first, bounded to the configured window.
    messages: Vec<StoredMessage>,
    /// The rolling summary of conversation older than the window, when one has
    /// been produced.
    summary: Option<String>,
}

impl ChatState {
    /// An empty state for `chat`.
    fn new(chat: &ChatId) -> Self {
        Self {
            chat: chat.clone(),
            messages: Vec::new(),
            summary: None,
        }
    }
}

/// The per-chat message store.
pub struct MessageStore {
    dir: PathBuf,
    /// The window each chat is pruned to.
    window: usize,
    state: Mutex<HashMap<ChatId, ChatState>>,
}

impl MessageStore {
    /// Opens the store under `dir`, reading whatever chat files are there.
    ///
    /// `":memory:"` keeps the state only in this store. A missing directory is
    /// created on the first write; an unreadable or unparseable file is an
    /// error, so a corrupt store fails at startup rather than on a later message.
    pub fn open(dir: impl Into<PathBuf>, window: usize) -> Result<Self, String> {
        let dir = dir.into();
        let mut state = HashMap::new();
        if dir.as_os_str() != ":memory:" && dir.exists() {
            let entries = std::fs::read_dir(&dir)
                .map_err(|error| format!("Failed to read {}: {error}", dir.display()))?;
            for entry in entries {
                let path = entry
                    .map_err(|error| format!("Failed to read {}: {error}", dir.display()))?
                    .path();
                if path.extension().is_none_or(|ext| ext != "json") {
                    continue;
                }
                let text = std::fs::read_to_string(&path)
                    .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
                let chat: ChatState = serde_json::from_str(&text)
                    .map_err(|error| format!("Failed to parse {}: {error}", path.display()))?;
                state.insert(chat.chat.clone(), chat);
            }
        }
        Ok(Self {
            dir,
            window,
            state: Mutex::new(state),
        })
    }

    /// Appends `message` to `chat`, unless that message id is already stored.
    ///
    /// Returns whether the message was new. Appending an id that is present
    /// changes nothing, which is what makes redelivery and offline replay
    /// idempotent — and the caller uses the "not new" answer to skip a second
    /// reply. The window is pruned to its bound after every append.
    pub fn append(&self, chat: &ChatId, message: StoredMessage) -> Result<bool, String> {
        let mut state = self.state()?;
        let entry = state
            .entry(chat.clone())
            .or_insert_with(|| ChatState::new(chat));
        if entry
            .messages
            .iter()
            .any(|existing| existing.message_id == message.message_id)
        {
            return Ok(false);
        }
        entry.messages.push(message);
        if entry.messages.len() > self.window {
            let excess = entry.messages.len() - self.window;
            entry.messages.drain(0..excess);
        }
        let updated = entry.clone();
        drop(state);
        self.write(chat, &updated)?;
        Ok(true)
    }

    /// The last `limit` messages in `chat`, oldest first.
    ///
    /// `limit` is clamped to the stored window; passing a larger number returns
    /// everything the store holds.
    pub fn recent(&self, chat: &ChatId, limit: usize) -> Result<Vec<StoredMessage>, String> {
        let state = self.state()?;
        let messages = match state.get(chat) {
            Some(chat) => chat.messages.as_slice(),
            None => &[],
        };
        let start = messages.len().saturating_sub(limit);
        Ok(messages[start..].to_vec())
    }

    /// When `chat` last said anything, or `None` when it has no history.
    ///
    /// A private turn uses this to decide whether the conversation is continuing
    /// or a new session has begun after a long gap.
    pub fn last_message_time(&self, chat: &ChatId) -> Result<Option<DateTime<Utc>>, String> {
        Ok(self
            .state()?
            .get(chat)
            .and_then(|chat| chat.messages.last())
            .map(|message| message.timestamp))
    }

    /// The rolling summary of `chat`, when one has been produced.
    pub fn summary(&self, chat: &ChatId) -> Result<Option<String>, String> {
        Ok(self
            .state()?
            .get(chat)
            .and_then(|chat| chat.summary.clone()))
    }

    /// Replaces `chat`'s rolling summary.
    pub fn set_summary(&self, chat: &ChatId, summary: impl Into<String>) -> Result<(), String> {
        let mut state = self.state()?;
        let entry = state
            .entry(chat.clone())
            .or_insert_with(|| ChatState::new(chat));
        entry.summary = Some(summary.into());
        let updated = entry.clone();
        drop(state);
        self.write(chat, &updated)
    }

    /// How many messages `chat` currently holds, for tests and diagnostics.
    pub fn len(&self, chat: &ChatId) -> Result<usize, String> {
        Ok(self
            .state()?
            .get(chat)
            .map_or(0, |chat| chat.messages.len()))
    }

    /// Whether `chat` holds no messages.
    pub fn is_empty(&self, chat: &ChatId) -> Result<bool, String> {
        Ok(self.len(chat)? == 0)
    }

    fn state(&self) -> Result<std::sync::MutexGuard<'_, HashMap<ChatId, ChatState>>, String> {
        self.state
            .lock()
            .map_err(|_| "the message store lock was poisoned".to_string())
    }

    /// Writes one chat's file from scratch, atomically.
    fn write(&self, chat: &ChatId, state: &ChatState) -> Result<(), String> {
        if self.dir.as_os_str() == ":memory:" {
            return Ok(());
        }
        std::fs::create_dir_all(&self.dir)
            .map_err(|error| format!("Failed to create {}: {error}", self.dir.display()))?;

        let path = self.dir.join(format!("{}.json", encode(chat.as_str())));
        let text = serde_json::to_string(state)
            .map_err(|error| format!("Failed to encode {}: {error}", chat.as_str()))?;
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, text)
            .map_err(|error| format!("Failed to write {}: {error}", temporary.display()))?;
        std::fs::rename(&temporary, &path).map_err(|error| {
            format!(
                "Failed to replace {} with {}: {error}",
                path.display(),
                temporary.display()
            )
        })
    }
}

/// Encodes a chat id into a filename-safe stem.
///
/// A chat id is opaque to the agent, so it may contain characters a filesystem
/// does not like. Everything outside a small safe set is percent-encoded; the
/// id itself travels inside the file, so the name never has to be decoded.
fn encode(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for byte in id.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b'_' | b'@' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: &str, text: &str) -> StoredMessage {
        StoredMessage {
            message_id: id.into(),
            sender: SenderId::new("u"),
            sender_name: None,
            text: Some(text.into()),
            from_self: false,
            timestamp: Utc::now(),
        }
    }

    #[test]
    fn appending_the_same_id_twice_stores_one_message() {
        let store = MessageStore::open(":memory:", 200).unwrap();
        let chat = ChatId::new("gA");
        store.append(&chat, message("m1", "hi")).unwrap();
        store.append(&chat, message("m1", "hi")).unwrap();
        assert_eq!(store.len(&chat).unwrap(), 1);
    }

    #[test]
    fn the_window_prunes_the_oldest_messages() {
        let store = MessageStore::open(":memory:", 3).unwrap();
        let chat = ChatId::new("gA");
        for i in 0..5 {
            store.append(&chat, message(&format!("m{i}"), "x")).unwrap();
        }
        let recent = store.recent(&chat, 10).unwrap();
        assert_eq!(recent.len(), 3);
        assert_eq!(recent.first().unwrap().message_id, "m2");
        assert_eq!(recent.last().unwrap().message_id, "m4");
    }

    #[test]
    fn recent_returns_the_last_n_oldest_first() {
        let store = MessageStore::open(":memory:", 200).unwrap();
        let chat = ChatId::new("gA");
        for i in 0..5 {
            store.append(&chat, message(&format!("m{i}"), "x")).unwrap();
        }
        let recent = store.recent(&chat, 2).unwrap();
        assert_eq!(
            recent
                .iter()
                .map(|m| m.message_id.as_str())
                .collect::<Vec<_>>(),
            ["m3", "m4"]
        );
    }

    #[test]
    fn a_file_survives_reopening() {
        let dir = std::env::temp_dir().join(format!("megumi-agent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let chat = ChatId::new("120363@g.us");
        let store = MessageStore::open(&dir, 200).unwrap();
        store.append(&chat, message("m1", "hello")).unwrap();
        store.set_summary(&chat, "they greeted each other").unwrap();
        drop(store);

        let reopened = MessageStore::open(&dir, 200).unwrap();
        assert_eq!(reopened.len(&chat).unwrap(), 1);
        assert_eq!(
            reopened.summary(&chat).unwrap().as_deref(),
            Some("they greeted each other")
        );
        assert!(reopened.last_message_time(&chat).unwrap().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
