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

/// What a stored record is.
///
/// A `Fact` is what someone stated; a `Reflection` is a higher-level insight
/// the consolidation pass derived by combining facts. The distinction matters in
/// the prompt: an insight is the agent's own inference, so [`render_memory`]
/// marks it as such and the model does not read it as ground truth. The serde
/// representation is externally tagged with no `other` fallback, the same as
/// [`Visibility`]: a label the code does not know fails to open rather than
/// being read as a default.
///
/// [`render_memory`]: crate::context::render_memory
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemoryKind {
    /// Something a participant stated.
    #[default]
    Fact,
    /// An insight the consolidation pass derived from facts.
    Reflection,
}

/// One durable fact, with provenance and a validity window.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct MemoryRecord {
    /// A stable id, used by a later correction to name the record it replaces.
    pub id: String,
    /// Whether this is a stated fact or a derived reflection.
    ///
    /// Defaulted so a memory file written before the field existed still loads
    /// as facts, which is what everything was then.
    #[serde(default)]
    pub kind: MemoryKind,
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
    /// Whether the fact was explicitly forgotten.
    ///
    /// A forgotten fact is never recalled — not even for a question about the
    /// past — so this is distinct from `valid_to`, which closes a fact's window
    /// but leaves it answerable as history. The record stays on disk (nothing is
    /// deleted) with its provenance intact; only its recall is switched off.
    /// Defaulted so a memory file written before the field existed still loads.
    #[serde(default)]
    pub forgotten: bool,
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
    /// Store a derived insight as a [`MemoryKind::Reflection`] record.
    ///
    /// The insight is built like a fact — the writer, not the model, fills
    /// every privilege and time field — but its `evidence` names the **stored
    /// fact ids** it was derived from rather than message ids, so the record's
    /// `source_message_ids` traces the inference back to the facts behind it.
    Reflect(NewFact),
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
    /// Mark `target` forgotten, so it is never recalled again.
    ///
    /// Distinct from [`Invalidate`](Self::Invalidate): an invalidated fact is
    /// still answerable as history for a question about the past, while a
    /// forgotten one is not. Nothing is deleted — the record keeps its
    /// provenance and only its recall is switched off.
    Forget {
        /// The id of the record to forget.
        target: String,
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
    /// Chat id → the id of the newest fact its last consolidation pass saw.
    ///
    /// The consolidation pass has its own cursor, separate from the extraction
    /// cursor: a pass that stores nothing (a `NOOP`, or an unreadable reply)
    /// still advances it, so a chat is not asked to reflect again until new
    /// facts have landed. Defaulted so a memory file written before the pass
    /// existed still loads.
    #[serde(default)]
    reflection_cursor: HashMap<ChatId, String>,
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
        self.apply_with(chat, ops, cursor, None)
    }

    /// Applies a consolidation pass's `ops` and advances the reflection cursor.
    ///
    /// The mirror of [`apply`](Self::apply) for the reflection pass: the ops are
    /// the same shape, but the cursor that moves is the reflection watermark, so
    /// a pass never disturbs where extraction left off. The two cursors are
    /// written together, so a pass is still one atomic write.
    pub fn apply_reflections(
        &self,
        chat: &ChatId,
        ops: &[MemoryOp],
        cursor: Option<String>,
    ) -> Result<usize, String> {
        self.apply_with(chat, ops, None, cursor)
    }

    /// Applies `ops` and advances whichever cursors are given, in one write.
    fn apply_with(
        &self,
        chat: &ChatId,
        ops: &[MemoryOp],
        extract_cursor: Option<String>,
        reflect_cursor: Option<String>,
    ) -> Result<usize, String> {
        let mut state = self.state()?;
        let mut updated = state.clone();
        let mut applied = 0;
        for op in ops {
            match op {
                MemoryOp::Noop => {}
                MemoryOp::Add(fact) => {
                    updated
                        .records
                        .push(MemoryRecord::from_fact(chat, fact, MemoryKind::Fact));
                    applied += 1;
                }
                MemoryOp::Reflect(fact) => {
                    updated.records.push(MemoryRecord::from_fact(
                        chat,
                        fact,
                        MemoryKind::Reflection,
                    ));
                    applied += 1;
                }
                MemoryOp::Update { target, fact } => {
                    let new = MemoryRecord::from_fact(chat, fact, MemoryKind::Fact);
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
                MemoryOp::Forget { target } => {
                    let Some(old) = updated.records.iter_mut().find(|r| r.id == *target) else {
                        continue;
                    };
                    old.forgotten = true;
                    applied += 1;
                }
            }
        }
        if let Some(cursor) = extract_cursor {
            updated.cursor.insert(chat.clone(), cursor);
        }
        if let Some(cursor) = reflect_cursor {
            updated.reflection_cursor.insert(chat.clone(), cursor);
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

    /// The id of the newest fact `chat`'s last consolidation pass saw.
    pub fn reflection_cursor(&self, chat: &ChatId) -> Result<Option<String>, String> {
        Ok(self.state()?.reflection_cursor.get(chat).cloned())
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
    /// was not learned in. `kind` is passed by the caller because the op, not
    /// the fact, decides whether a record is a stated fact or a reflection.
    fn from_fact(chat: &ChatId, fact: &NewFact, kind: MemoryKind) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            kind,
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
            forgotten: false,
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
    fn a_forget_flags_the_record_without_closing_its_window() {
        let store = MemoryStore::open(":memory:").unwrap();
        let chat = ChatId::new("gA");
        store
            .apply(&chat, &[MemoryOp::Add(fact("a fact"))], None)
            .unwrap();
        let id = store.records().unwrap()[0].id.clone();
        store
            .apply(&chat, &[MemoryOp::Forget { target: id.clone() }], None)
            .unwrap();

        let record = store.get(&id).unwrap().unwrap();
        assert!(record.forgotten);
        // Forget is not invalidate: the window stays open, only recall is off.
        assert!(record.valid_to.is_none());
        assert!(record.superseded_by.is_none());
    }

    #[test]
    fn a_forgotten_flag_survives_reopening() {
        let path = std::env::temp_dir().join(format!("megumi-forget-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let chat = ChatId::new("gA");
        let store = MemoryStore::open(&path).unwrap();
        store
            .apply(&chat, &[MemoryOp::Add(fact("a fact"))], None)
            .unwrap();
        let id = store.records().unwrap()[0].id.clone();
        store
            .apply(&chat, &[MemoryOp::Forget { target: id.clone() }], None)
            .unwrap();
        drop(store);

        let reopened = MemoryStore::open(&path).unwrap();
        assert!(reopened.get(&id).unwrap().unwrap().forgotten);
        let _ = std::fs::remove_file(&path);
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
    fn a_reflect_op_stores_a_reflection_record() {
        let store = MemoryStore::open(":memory:").unwrap();
        let chat = ChatId::new("gA");
        store
            .apply(&chat, &[MemoryOp::Add(fact("a stated fact"))], None)
            .unwrap();
        store
            .apply_reflections(
                &chat,
                &[MemoryOp::Reflect(fact("an inferred insight"))],
                None,
            )
            .unwrap();

        let records = store.records().unwrap();
        assert_eq!(records[0].kind, MemoryKind::Fact);
        assert_eq!(records[1].kind, MemoryKind::Reflection);
        // The evidence a reflection carries names the facts it came from.
        assert_eq!(records[1].source_message_ids, ["m1"]);
    }

    #[test]
    fn the_two_cursors_move_independently() {
        let store = MemoryStore::open(":memory:").unwrap();
        let chat = ChatId::new("gA");
        // An extraction pass moves only the extraction cursor...
        store
            .apply(&chat, &[MemoryOp::Noop], Some("m5".into()))
            .unwrap();
        assert_eq!(store.cursor(&chat).unwrap().as_deref(), Some("m5"));
        assert!(store.reflection_cursor(&chat).unwrap().is_none());

        // ...and a consolidation pass moves only the reflection cursor.
        store
            .apply_reflections(&chat, &[MemoryOp::Noop], Some("f2".into()))
            .unwrap();
        assert_eq!(store.cursor(&chat).unwrap().as_deref(), Some("m5"));
        assert_eq!(
            store.reflection_cursor(&chat).unwrap().as_deref(),
            Some("f2")
        );
    }

    #[test]
    fn a_record_written_before_kind_existed_loads_as_a_fact() {
        // An old memory file has no `kind` field; every record in it was a fact,
        // so the default must read it as one rather than fail to open.
        let path = std::env::temp_dir().join(format!("megumi-oldmem-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let text = r#"{"records":[{"id":"r1","content":"a fact","valid_from":"2026-01-01T00:00:00Z",
            "valid_to":null,"recorded_at":"2026-01-01T00:00:00Z","superseded_by":null,
            "source_message_ids":["m1"],"confidence":0.9,"importance":0.5,"visibility":"Chat",
            "origin_chat":"gA","subject":null,"forgotten":false}]}"#;
        std::fs::write(&path, text).unwrap();

        let store = MemoryStore::open(&path).unwrap();
        assert_eq!(store.records().unwrap()[0].kind, MemoryKind::Fact);
        let _ = std::fs::remove_file(&path);
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
        let record = MemoryRecord::from_fact(&ChatId::new("gA"), &fact("a fact"), MemoryKind::Fact);
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
