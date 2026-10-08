//! The memory store: durable facts, their validity windows, and the extraction
//! cursor.
//!
//! A fact is a [`MemoryRecord`] — self-contained, in the third person, with the
//! messages it came from and the window over which it was true. The store is one
//! JSON file for the whole agent, written whole and atomically the way the
//! message store and the news store are, so a crash mid-write cannot leave a
//! half-written file. `":memory:"` keeps everything in the store and touches no
//! disk, which is what the tests use.
//!
//! A fact is never edited in place. Correcting one means adding a new record and
//! closing the old one's window, so the old value stays answerable as history —
//! that is the whole point of the bi-temporal fields. [`MemoryStore::apply`] is
//! the only method that mutates anything, and it takes a batch of [`MemoryOp`]s
//! so a pass is one atomic write.
//!
//! The cursor is the id of the last message a chat's extraction pass consumed.
//! It is stored beside the records because it must survive a restart: without
//! it, a restart would re-extract the whole window.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::context::Visibility;
use crate::event::{ChatId, SenderId};

/// One durable fact, with provenance and a validity window.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct MemoryRecord {
    /// A stable id, used by a later correction to name the record it replaces.
    pub id: String,
    /// The fact, self-contained and in the third person.
    pub content: String,
    /// When the fact became true.
    pub valid_from: DateTime<Utc>,
    /// When it stopped being true, or `None` while it still holds.
    pub valid_to: Option<DateTime<Utc>>,
    /// When the agent recorded the fact, which may be long after it was true.
    pub recorded_at: DateTime<Utc>,
    /// The id of the record that replaced this one, when one did.
    pub superseded_by: Option<String>,
    /// The messages the fact was extracted from.
    pub source_message_ids: Vec<String>,
    /// How sure the extraction was, in `0.0..=1.0`.
    pub confidence: f32,
    /// How worth recalling the fact is, in `0.0..=1.0`.
    pub importance: f32,
    /// Where the fact may be shown.
    pub visibility: Visibility,
    /// The chat the fact was learned in.
    pub origin_chat: ChatId,
    /// The person the fact is about, when it is about someone.
    pub subject: Option<SenderId>,
}

/// A fact the writer has validated and wants stored.
///
/// The writer, not the model, fills every field here: `visibility` is derived
/// from the chat the fact was learned in, and `valid_from` from the evidence
/// messages' timestamps, so the model can never widen a fact's reach or move it
/// in time.
#[derive(Clone, Debug)]
pub struct NewFact {
    /// The fact, self-contained and in the third person.
    pub content: String,
    /// Where the fact may be shown.
    pub visibility: Visibility,
    /// The person the fact is about, when it is about someone.
    pub subject: Option<SenderId>,
    /// How sure the extraction was, in `0.0..=1.0`.
    pub confidence: f32,
    /// How worth recalling the fact is, in `0.0..=1.0`.
    pub importance: f32,
    /// When the fact became true, from the earliest evidence message.
    pub valid_from: DateTime<Utc>,
    /// The messages the fact was extracted from.
    pub evidence: Vec<String>,
}

/// One change to apply to the store.
///
/// A pass produces a batch of these, and [`MemoryStore::apply`] runs the whole
/// batch in one write. `Noop` is explicit so the model can say "nothing here is
/// worth keeping" without the parser having to distinguish an empty list.
#[derive(Clone, Debug)]
pub enum MemoryOp {
    /// Store a new fact.
    Add(NewFact),
    /// Close `target`'s window and store `fact` as its replacement.
    Update {
        /// The id of the record being corrected.
        target: String,
        /// The corrected fact.
        fact: NewFact,
    },
    /// Close `target`'s window with nothing replacing it.
    Invalidate {
        /// The id of the record that stopped being true.
        target: String,
        /// When it stopped being true.
        valid_from: DateTime<Utc>,
    },
    /// Nothing worth keeping.
    Noop,
}

/// Everything the store keeps: the records and each chat's extraction cursor.
#[derive(Serialize, Deserialize, Default, Clone)]
struct State {
    #[serde(default)]
    records: Vec<MemoryRecord>,
    /// Chat id → the id of the last message its last pass consumed.
    #[serde(default)]
    cursor: HashMap<ChatId, String>,
}

/// The agent's durable memory.
pub struct MemoryStore {
    path: PathBuf,
    state: Mutex<State>,
}

impl MemoryStore {
    /// Opens the store at `path`, reading what is already there.
    ///
    /// `":memory:"` keeps the state only in this store. A missing file is an
    /// empty store, created on the first write; an unreadable or unparseable
    /// file is an error, so a corrupt memory file fails at startup rather than
    /// on a later message. An unknown `visibility` label is unparseable, which
    /// is deliberate: a label the code does not understand must not be read as
    /// one it does.
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

    /// Applies `ops` for `chat` in one atomic write, then advances the cursor.
    ///
    /// Returns how many ops changed something (`Noop`, and a `target` that no
    /// longer exists, do not count). The whole batch is applied to a copy and
    /// written before the in-memory state is replaced, so a write that fails
    /// leaves the store — and the cursor — as they were, and the pass is retried
    /// whole.
    pub fn apply(
        &self,
        chat: &ChatId,
        ops: &[MemoryOp],
        cursor: Option<String>,
    ) -> Result<usize, String> {
        let mut state = self.state()?;
        let mut updated = state.clone();
        let mut applied = 0;
        for op in ops {
            match op {
                MemoryOp::Noop => {}
                MemoryOp::Add(fact) => {
                    updated.records.push(MemoryRecord::from_fact(chat, fact));
                    applied += 1;
                }
                MemoryOp::Update { target, fact } => {
                    let new = MemoryRecord::from_fact(chat, fact);
                    let Some(old) = updated.records.iter_mut().find(|r| r.id == *target) else {
                        continue;
                    };
                    old.valid_to = Some(fact.valid_from);
                    old.superseded_by = Some(new.id.clone());
                    updated.records.push(new);
                    applied += 1;
                }
                MemoryOp::Invalidate { target, valid_from } => {
                    let Some(old) = updated.records.iter_mut().find(|r| r.id == *target) else {
                        continue;
                    };
                    old.valid_to = Some(*valid_from);
                    applied += 1;
                }
            }
        }
        if let Some(cursor) = cursor {
            updated.cursor.insert(chat.clone(), cursor);
        }
        self.write(&updated)?;
        *state = updated;
        Ok(applied)
    }

    /// A copy of every record, in insertion order.
    pub fn records(&self) -> Result<Vec<MemoryRecord>, String> {
        Ok(self.state()?.records.clone())
    }

    /// One record by id, when it exists.
    pub fn get(&self, id: &str) -> Result<Option<MemoryRecord>, String> {
        Ok(self
            .state()?
            .records
            .iter()
            .find(|record| record.id == id)
            .cloned())
    }

    /// The id of the last message `chat`'s extraction consumed.
    pub fn cursor(&self, chat: &ChatId) -> Result<Option<String>, String> {
        Ok(self.state()?.cursor.get(chat).cloned())
    }

    /// How many records the store holds.
    pub fn len(&self) -> Result<usize, String> {
        Ok(self.state()?.records.len())
    }

    /// Whether the store holds no records.
    pub fn is_empty(&self) -> Result<bool, String> {
        Ok(self.len()? == 0)
    }

    fn state(&self) -> Result<std::sync::MutexGuard<'_, State>, String> {
        self.state
            .lock()
            .map_err(|_| "the memory store lock was poisoned".to_string())
    }

    /// Writes the whole store, atomically.
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
        let text = serde_json::to_string(state)
            .map_err(|error| format!("Failed to encode the memory store: {error}"))?;
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

impl MemoryRecord {
    /// Builds a record from a validated fact, stamping the id and the time.
    ///
    /// `chat` becomes the origin, so a fact cannot be attributed to a chat it
    /// was not learned in.
    fn from_fact(chat: &ChatId, fact: &NewFact) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            content: fact.content.clone(),
            valid_from: fact.valid_from,
            valid_to: None,
            recorded_at: Utc::now(),
            superseded_by: None,
            source_message_ids: fact.evidence.clone(),
            confidence: fact.confidence,
            importance: fact.importance,
            visibility: fact.visibility,
            origin_chat: chat.clone(),
            subject: fact.subject.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fact(content: &str) -> NewFact {
        NewFact {
            content: content.into(),
            visibility: Visibility::Chat,
            subject: None,
            confidence: 0.8,
            importance: 0.6,
            valid_from: Utc::now(),
            evidence: vec!["m1".into()],
        }
    }

    #[test]
    fn apply_lands_ops_and_advances_the_cursor() {
        let store = MemoryStore::open(":memory:").unwrap();
        let chat = ChatId::new("gA");
        let applied = store
            .apply(
                &chat,
                &[MemoryOp::Add(fact("the venue is the old hall"))],
                Some("m1".into()),
            )
            .unwrap();
        assert_eq!(applied, 1);
        assert_eq!(store.len().unwrap(), 1);
        assert_eq!(store.cursor(&chat).unwrap().as_deref(), Some("m1"));
        let record = &store.records().unwrap()[0];
        assert_eq!(record.origin_chat, chat);
        assert_eq!(record.source_message_ids, ["m1"]);
        assert!(record.valid_to.is_none());
    }

    #[test]
    fn an_update_closes_the_old_window_and_points_at_the_new_record() {
        let store = MemoryStore::open(":memory:").unwrap();
        let chat = ChatId::new("gA");
        store
            .apply(
                &chat,
                &[MemoryOp::Add(fact("the venue is the old hall"))],
                None,
            )
            .unwrap();
        let old_id = store.records().unwrap()[0].id.clone();

        let later = Utc::now();
        let mut corrected = fact("the venue is the new hall");
        corrected.valid_from = later;
        store
            .apply(
                &chat,
                &[MemoryOp::Update {
                    target: old_id.clone(),
                    fact: corrected,
                }],
                None,
            )
            .unwrap();

        let old = store.get(&old_id).unwrap().unwrap();
        assert_eq!(old.valid_to, Some(later));
        let new_id = old.superseded_by.clone().unwrap();
        assert_eq!(store.get(&new_id).unwrap().unwrap().valid_to, None);
    }

    #[test]
    fn an_invalidate_closes_the_window_with_no_replacement() {
        let store = MemoryStore::open(":memory:").unwrap();
        let chat = ChatId::new("gA");
        store
            .apply(
                &chat,
                &[MemoryOp::Add(fact("the venue is the old hall"))],
                None,
            )
            .unwrap();
        let id = store.records().unwrap()[0].id.clone();
        let when = Utc::now();
        store
            .apply(
                &chat,
                &[MemoryOp::Invalidate {
                    target: id.clone(),
                    valid_from: when,
                }],
                None,
            )
            .unwrap();
        let record = store.get(&id).unwrap().unwrap();
        assert_eq!(record.valid_to, Some(when));
        assert!(record.superseded_by.is_none());
    }

    #[test]
    fn an_op_targeting_a_missing_record_is_skipped() {
        let store = MemoryStore::open(":memory:").unwrap();
        let chat = ChatId::new("gA");
        let applied = store
            .apply(
                &chat,
                &[
                    MemoryOp::Update {
                        target: "nope".into(),
                        fact: fact("x"),
                    },
                    MemoryOp::Add(fact("y")),
                ],
                None,
            )
            .unwrap();
        assert_eq!(applied, 1);
        assert_eq!(store.len().unwrap(), 1);
    }

    #[test]
    fn a_noop_changes_nothing_but_still_advances_the_cursor() {
        let store = MemoryStore::open(":memory:").unwrap();
        let chat = ChatId::new("gA");
        let applied = store
            .apply(&chat, &[MemoryOp::Noop], Some("m9".into()))
            .unwrap();
        assert_eq!(applied, 0);
        assert!(store.is_empty().unwrap());
        assert_eq!(store.cursor(&chat).unwrap().as_deref(), Some("m9"));
    }

    #[test]
    fn a_file_survives_reopening() {
        let path = std::env::temp_dir().join(format!("megumi-memory-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let chat = ChatId::new("120363@g.us");
        let store = MemoryStore::open(&path).unwrap();
        store
            .apply(
                &chat,
                &[MemoryOp::Add(fact("the venue is the old hall"))],
                Some("m1".into()),
            )
            .unwrap();
        drop(store);

        let reopened = MemoryStore::open(&path).unwrap();
        assert_eq!(reopened.len().unwrap(), 1);
        assert_eq!(reopened.cursor(&chat).unwrap().as_deref(), Some("m1"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_unknown_visibility_label_fails_to_open() {
        // A label the code does not know must be an error, never a visible
        // default: reading it as, say, `Chat` could leak a private fact. The
        // record is otherwise complete, so the only reason to fail is the label.
        let path = std::env::temp_dir().join(format!("megumi-badmem-{}.json", std::process::id()));
        let record = MemoryRecord::from_fact(&ChatId::new("gA"), &fact("a fact"));
        let json = serde_json::to_string(&record).unwrap();
        let text = format!(
            "{{\"records\":[{}]}}",
            json.replace("\"Chat\"", "\"Everyone\"")
        );
        std::fs::write(&path, text).unwrap();
        assert!(MemoryStore::open(&path).is_err());
        let _ = std::fs::remove_file(&path);
    }
}
