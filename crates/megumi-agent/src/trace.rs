//! A replayable record of every turn.
//!
//! The design's rule is that everything is logged and replayable: for each turn
//! the agent records the trigger, a hash of the context it built, the model and
//! token counts, the latency, and the reply. [`TraceSink`] keeps a bounded log,
//! oldest first, and persists it the same atomic way the message store does, so
//! a run can be inspected after the fact. The context itself is summarised by a
//! hash and its component sizes rather than copied, so the log stays small.

use std::path::PathBuf;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::event::ChatId;
use crate::gate::Trigger;

/// One turn, as recorded for replay.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TurnTrace {
    /// A unique id for this turn.
    pub turn_id: String,
    /// The chat the turn happened in.
    pub chat: ChatId,
    /// Why the agent spoke, as a stable label.
    pub trigger: String,
    /// The model that was called.
    pub model: String,
    /// A hash of the assembled context, so two turns can be compared.
    pub context_hash: u64,
    /// The size of the system prompt, in characters.
    pub system_chars: usize,
    /// The size of the user turn, in characters.
    pub user_chars: usize,
    /// Input tokens the model reported.
    pub input_tokens: Option<u32>,
    /// Output tokens the model reported.
    pub output_tokens: Option<u32>,
    /// How long the model call took.
    pub latency_ms: u64,
    /// The reply the agent decided on, or `None` when it stayed silent.
    pub reply: Option<String>,
    /// When the turn finished.
    pub timestamp: DateTime<Utc>,
}

impl TurnTrace {
    /// The stable label for a [`Trigger`], for the trace and for tests.
    pub fn trigger_label(trigger: Trigger) -> &'static str {
        match trigger {
            Trigger::Mention => "mention",
            Trigger::Reply => "reply",
            Trigger::PrivateMessage => "private",
        }
    }
}

/// The bounded, persistent log of turns.
pub struct TraceSink {
    path: PathBuf,
    capacity: usize,
    state: Mutex<Vec<TurnTrace>>,
}

impl TraceSink {
    /// Opens the log at `path`, reading what is already there.
    ///
    /// `":memory:"` keeps the log in the sink only, which is what the tests use.
    pub fn open(path: impl Into<PathBuf>, capacity: usize) -> Result<Self, String> {
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
            capacity,
            state: Mutex::new(state),
        })
    }

    /// Records `trace`, dropping the oldest once the log is full.
    pub fn record(&self, trace: TurnTrace) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "the trace lock was poisoned".to_string())?;
        state.push(trace);
        if state.len() > self.capacity {
            let excess = state.len() - self.capacity;
            state.drain(0..excess);
        }
        let updated = state.clone();
        drop(state);
        self.write(&updated)
    }

    /// The last `limit` traces, oldest first.
    pub fn recent(&self, limit: usize) -> Result<Vec<TurnTrace>, String> {
        let state = self
            .state
            .lock()
            .map_err(|_| "the trace lock was poisoned".to_string())?;
        let start = state.len().saturating_sub(limit);
        Ok(state[start..].to_vec())
    }

    /// How many turns the log holds.
    pub fn len(&self) -> Result<usize, String> {
        Ok(self
            .state
            .lock()
            .map_err(|_| "the trace lock was poisoned".to_string())?
            .len())
    }

    /// Whether the log holds no turns.
    pub fn is_empty(&self) -> Result<bool, String> {
        Ok(self.len()? == 0)
    }

    fn write(&self, state: &[TurnTrace]) -> Result<(), String> {
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
            .map_err(|error| format!("Failed to encode the trace log: {error}"))?;
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

/// A stable hash of the context, so a trace can be compared across runs.
///
/// FNV-1a over the bytes; the exact algorithm need not be stable across
/// versions, only within one, which is all a replay comparison needs.
pub fn context_hash(system: &str, user: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in system.bytes().chain(std::iter::once(0)).chain(user.bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trace(id: &str) -> TurnTrace {
        TurnTrace {
            turn_id: id.into(),
            chat: ChatId::new("gA"),
            trigger: "mention".into(),
            model: "claude-opus-5-5".into(),
            context_hash: 1,
            system_chars: 10,
            user_chars: 20,
            input_tokens: Some(5),
            output_tokens: Some(3),
            latency_ms: 100,
            reply: Some("hi".into()),
            timestamp: Utc::now(),
        }
    }

    #[test]
    fn the_log_is_bounded() {
        let sink = TraceSink::open(":memory:", 2).unwrap();
        for i in 0..4 {
            sink.record(trace(&format!("t{i}"))).unwrap();
        }
        let recent = sink.recent(10).unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent.first().unwrap().turn_id, "t2");
        assert_eq!(recent.last().unwrap().turn_id, "t3");
    }

    #[test]
    fn the_log_survives_reopening() {
        let path = std::env::temp_dir().join(format!("megumi-traces-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let sink = TraceSink::open(&path, 500).unwrap();
        sink.record(trace("t1")).unwrap();
        drop(sink);

        let reopened = TraceSink::open(&path, 500).unwrap();
        assert_eq!(reopened.len().unwrap(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_context_hash_is_stable_and_sensitive() {
        assert_eq!(context_hash("a", "b"), context_hash("a", "b"));
        assert_ne!(context_hash("a", "b"), context_hash("a", "c"));
        assert_ne!(context_hash("a", "b"), context_hash("ab", ""));
    }
}
