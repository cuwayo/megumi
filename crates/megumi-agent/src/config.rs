//! Every threshold, budget, and path the agent runs with, in one place.
//!
//! Nothing here is hard-coded into the pipeline: the agent reads its settings
//! from an [`AgentConfig`], and the bot builds one from the environment with
//! [`AgentConfig::from_env`]. That keeps the defaults in the design's cheat
//! sheet tunable without a rebuild, and lets a test build a small, deterministic
//! configuration with [`AgentConfig::for_test`].

use std::path::PathBuf;
use std::time::Duration;

/// The tuning knobs the agent runs with.
#[derive(Clone, Debug)]
pub struct AgentConfig {
    /// How many recent messages a group turn's context carries.
    pub group_window: usize,
    /// How many recent messages a private turn's context carries.
    pub private_window: usize,
    /// The ceiling on the assembled working context, in estimated tokens.
    ///
    /// The window sizes above bound the message count; this bounds the tokens,
    /// so a few long messages cannot blow past the model's input.
    pub max_context_tokens: usize,
    /// The most tokens the model may produce in one reply.
    pub max_reply_tokens: u32,
    /// The model id to call.
    pub model: String,
    /// The base URL of the Messages API.
    pub api_base: String,
    /// How long a private conversation may pause before the next message is
    /// treated as a new session.
    pub session_gap: Duration,
    /// How many messages to keep per chat on disk.
    pub max_stored_messages: usize,
    /// How many turns to keep in the trace log.
    pub trace_capacity: usize,
    /// Where the per-chat message files live, or `":memory:"` for none.
    pub store_dir: PathBuf,
    /// How many unprocessed messages trigger a memory extraction pass.
    pub memory_extract_batch: usize,
    /// How long a chat may sit with an unprocessed backlog before a pass.
    pub memory_extract_idle: Duration,
    /// The most messages one extraction pass reads.
    pub memory_extract_max_messages: usize,
    /// The most tokens an extraction reply may use.
    pub memory_extract_tokens: u32,
    /// The most existing facts an extraction prompt shows for targeting.
    pub memory_extract_max_memories: usize,
    /// How many memories a turn recalls into the prompt.
    pub memory_recall_top: usize,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            group_window: 30,
            private_window: 40,
            max_context_tokens: 6_000,
            max_reply_tokens: 1_024,
            model: "claude-sonnet-5-5".to_string(),
            api_base: "https://api.anthropic.com".to_string(),
            session_gap: Duration::from_secs(6 * 60 * 60),
            max_stored_messages: 200,
            trace_capacity: 500,
            store_dir: PathBuf::from("agent"),
            memory_extract_batch: 25,
            memory_extract_idle: Duration::from_secs(10 * 60),
            memory_extract_max_messages: 50,
            memory_extract_tokens: 1_024,
            memory_extract_max_memories: 50,
            memory_recall_top: 8,
        }
    }
}

impl AgentConfig {
    /// Reads the environment, falling back to [`Default`] for anything unset.
    ///
    /// Every variable is optional, matching the bot's other settings: without
    /// them the agent runs on the design's default budgets. An unparseable value
    /// is ignored rather than fatal, so a typo cannot stop the bot from starting.
    pub fn from_env() -> Self {
        let default = Self::default();
        Self {
            group_window: env_usize("AGENT_GROUP_WINDOW").unwrap_or(default.group_window),
            private_window: env_usize("AGENT_PRIVATE_WINDOW").unwrap_or(default.private_window),
            max_context_tokens: env_usize("AGENT_MAX_CONTEXT_TOKENS")
                .unwrap_or(default.max_context_tokens),
            max_reply_tokens: env_usize("AGENT_MAX_REPLY_TOKENS")
                .map(|tokens| tokens as u32)
                .unwrap_or(default.max_reply_tokens),
            model: std::env::var("ANTHROPIC_MODEL").unwrap_or(default.model),
            // `ANTHROPIC_BASE_URL` is what Claude Code and most tooling set;
            // `ANTHROPIC_API_BASE` is kept as an alias.
            api_base: std::env::var("ANTHROPIC_BASE_URL")
                .or_else(|_| std::env::var("ANTHROPIC_API_BASE"))
                .unwrap_or(default.api_base),
            session_gap: env_usize("AGENT_SESSION_GAP")
                .map(|secs| Duration::from_secs(secs as u64))
                .unwrap_or(default.session_gap),
            max_stored_messages: env_usize("AGENT_STORE_WINDOW")
                .unwrap_or(default.max_stored_messages),
            trace_capacity: env_usize("AGENT_TRACE_CAPACITY").unwrap_or(default.trace_capacity),
            store_dir: std::env::var("AGENT_DIR")
                .map(PathBuf::from)
                .unwrap_or(default.store_dir),
            memory_extract_batch: env_usize("AGENT_MEMORY_EXTRACT_BATCH")
                .unwrap_or(default.memory_extract_batch),
            memory_extract_idle: env_usize("AGENT_MEMORY_EXTRACT_IDLE")
                .map(|secs| Duration::from_secs(secs as u64))
                .unwrap_or(default.memory_extract_idle),
            memory_extract_max_messages: env_usize("AGENT_MEMORY_EXTRACT_MAX_MESSAGES")
                .unwrap_or(default.memory_extract_max_messages),
            memory_extract_tokens: env_usize("AGENT_MEMORY_EXTRACT_TOKENS")
                .map(|tokens| tokens as u32)
                .unwrap_or(default.memory_extract_tokens),
            memory_extract_max_memories: env_usize("AGENT_MEMORY_EXTRACT_MAX_MEMORIES")
                .unwrap_or(default.memory_extract_max_memories),
            memory_recall_top: env_usize("AGENT_MEMORY_RECALL_TOP")
                .unwrap_or(default.memory_recall_top),
        }
    }

    /// A small, in-memory configuration for tests.
    ///
    /// Memory extraction is switched off — the batch threshold is unreachable
    /// and the idle gap is forever — so a test that is not about memory never
    /// spends a scripted model reply on an extraction pass. A memory test lowers
    /// the threshold itself.
    pub fn for_test() -> Self {
        Self {
            store_dir: PathBuf::from(":memory:"),
            memory_extract_batch: usize::MAX,
            memory_extract_idle: Duration::MAX,
            ..Self::default()
        }
    }

    /// The directory the per-chat message files live in.
    ///
    /// Kept separate from the trace log so [`MessageStore`](crate::MessageStore)
    /// can read every file in its directory as a chat. `":memory:"` stays
    /// `":memory:"`, so nothing is written.
    pub fn chats_dir(&self) -> PathBuf {
        if self.store_dir.as_os_str() == ":memory:" {
            PathBuf::from(":memory:")
        } else {
            self.store_dir.join("chats")
        }
    }

    /// The path of the turn log.
    ///
    /// Beside the chats directory, not inside it, so the message store never
    /// tries to read the trace log as a chat.
    pub fn trace_path(&self) -> PathBuf {
        if self.store_dir.as_os_str() == ":memory:" {
            PathBuf::from(":memory:")
        } else {
            self.store_dir.join("traces.json")
        }
    }

    /// The path of the memory store.
    ///
    /// Beside the chats directory and the trace log, for the same reason: the
    /// message store reads every file in `chats_dir` as a chat, so nothing else
    /// may live among them.
    pub fn memory_path(&self) -> PathBuf {
        if self.store_dir.as_os_str() == ":memory:" {
            PathBuf::from(":memory:")
        } else {
            self.store_dir.join("memories.json")
        }
    }
}

fn env_usize(key: &str) -> Option<usize> {
    std::env::var(key).ok()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_design_cheat_sheet() {
        let config = AgentConfig::default();
        assert_eq!(config.group_window, 30);
        assert_eq!(config.private_window, 40);
        assert_eq!(config.session_gap, Duration::from_secs(6 * 60 * 60));
        assert_eq!(config.model, "claude-sonnet-5-5");
    }

    #[test]
    fn for_test_stays_off_disk() {
        assert_eq!(AgentConfig::for_test().store_dir, PathBuf::from(":memory:"));
        assert_eq!(
            AgentConfig::for_test().chats_dir(),
            PathBuf::from(":memory:")
        );
        assert_eq!(
            AgentConfig::for_test().trace_path(),
            PathBuf::from(":memory:")
        );
    }

    #[test]
    fn the_trace_log_sits_outside_the_chats_directory() {
        // The message store reads every file in its directory as a chat, so the
        // trace log must not live among them.
        let config = AgentConfig::default();
        let chats = config.chats_dir();
        assert!(!config.trace_path().starts_with(&chats), "{chats:?}");
        assert!(!config.memory_path().starts_with(&chats), "{chats:?}");
    }

    #[test]
    fn for_test_never_runs_a_memory_pass() {
        // An unreachable batch and an infinite idle gap keep every non-memory
        // test from spending a scripted reply on extraction.
        let config = AgentConfig::for_test();
        assert_eq!(config.memory_extract_batch, usize::MAX);
        assert_eq!(config.memory_extract_idle, Duration::MAX);
        assert_eq!(config.memory_path(), PathBuf::from(":memory:"));
    }
}
