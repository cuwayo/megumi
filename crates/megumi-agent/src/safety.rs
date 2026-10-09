//! The safety layer: what may leave the building, and what a tool may do.
//!
//! Two things are enforced here, both in code rather than by asking the model
//! nicely, and both failing closed:
//!
//! - [`screen_reply`] is the output guard. A reply that carries the prompt's own
//!   trust tags, or a verbatim sentence of the system prompt, is the signature of
//!   a prompt injection that worked — the model parroting our scaffolding or our
//!   instructions. Such a reply is dropped rather than sent; an ordinary reply is
//!   truncated to a configured cap and sent. The context builder escapes the tags
//!   on the way in, so this is the second half of the same defence.
//! - [`PendingConfirmations`] holds a state-changing tool call between the
//!   model asking to make it and the user agreeing. A [`Tool`](crate::tools::Tool)
//!   whose [`confirmation`](crate::tools::Tool::confirmation) is `Some` is never
//!   run on the model's word alone; the question goes to the chat, and only a
//!   clear [`Confirmation::Yes`] runs it. The pending call is kept per chat, in
//!   memory, and expires after a configured TTL.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::config::AgentConfig;
use crate::context::truncate;
use crate::event::ChatId;

/// The literal reply a model uses to decline to speak.
const NO_REPLY: &str = "NO_REPLY";

/// The tags the context builder wraps conversation content in.
///
/// A reply that contains one is the model echoing our own scaffolding — the
/// mark of an injection that got through — so it is never sent. Matched
/// case-insensitively against the lowercased reply.
const INTERNAL_TAGS: &[&str] = &[
    "<chat_message",
    "</chat_message",
    "<memories",
    "</memories",
    "<conversation_summary",
    "</conversation_summary",
    "<plan",
    "</plan",
    "<reply_to",
    "</reply_to",
];

/// The shortest system-prompt sentence that counts as a leaked instruction.
///
/// A reply normally shares no 40-character run with the system prompt, so a
/// match is the model reciting its instructions rather than answering.
const MIN_LEAK_CHARS: usize = 40;

/// Whether `reply` may be sent, and the text to send.
///
/// Returns `None` — send nothing — for an empty reply, a `NO_REPLY`, a reply
/// that carries the prompt's internal tags, or one that recites the system
/// prompt. Otherwise the reply is trimmed and truncated to
/// [`AgentConfig::max_reply_chars`]. The guard is deliberately conservative: a
/// false suppression costs one unanswered turn, a false send can leak the prompt
/// or the model's instructions into a chat.
pub fn screen_reply(reply: &str, system: &str, config: &AgentConfig) -> Option<String> {
    let trimmed = reply.trim();
    if is_silent(trimmed) {
        return None;
    }
    let lower = trimmed.to_lowercase();
    if INTERNAL_TAGS.iter().any(|tag| lower.contains(tag)) || leaks_system_prompt(trimmed, system) {
        return None;
    }
    Some(truncate(trimmed, config.max_reply_chars))
}

/// Whether `reply` is one the agent treats as "say nothing".
///
/// An empty reply and a `NO_REPLY` both mean silence. Shared with the reasoning
/// layer, which must not spend an evaluator call judging a reply that will not
/// be sent.
pub(crate) fn is_silent(reply: &str) -> bool {
    let trimmed = reply.trim();
    trimmed.is_empty() || trimmed.eq_ignore_ascii_case(NO_REPLY)
}

/// Whether `reply` quotes a long run of the system prompt verbatim.
fn leaks_system_prompt(reply: &str, system: &str) -> bool {
    system
        .split(['.', '!', '?', '\n'])
        .map(str::trim)
        .filter(|sentence| sentence.chars().count() >= MIN_LEAK_CHARS)
        .any(|sentence| reply.contains(sentence))
}

/// What a user message means while a tool confirmation is pending.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confirmation {
    /// Run the held tool.
    Yes,
    /// Drop the held tool without running it.
    No,
    /// Neither: the message is not an answer, so the held tool stays pending
    /// until its TTL expires.
    Unrelated,
}

/// The words that accept a confirmation.
const YES_WORDS: &[&str] = &[
    "yes",
    "yeah",
    "yep",
    "yup",
    "y",
    "ok",
    "okay",
    "sure",
    "confirm",
    "confirmed",
    "proceed",
    "go ahead",
    "do it",
    "please do",
    "sounds good",
];

/// The words that decline a confirmation.
const NO_WORDS: &[&str] = &[
    "no",
    "nope",
    "nah",
    "n",
    "cancel",
    "stop",
    "don't",
    "dont",
    "never mind",
    "nevermind",
    "nvm",
];

/// Reads a message as an answer to a pending confirmation.
///
/// Cleaned the way [`gate`](crate::gate) cleans an acknowledgement — lowercased
/// and stripped of surrounding punctuation — and matched against a small fixed
/// word list, so a real question is never mistaken for a yes or a no.
pub fn classify_confirmation(text: &str) -> Confirmation {
    let cleaned = text
        .trim()
        .to_lowercase()
        .trim_matches(|ch: char| ch.is_whitespace() || ch.is_ascii_punctuation())
        .to_string();
    if YES_WORDS.contains(&cleaned.as_str()) {
        Confirmation::Yes
    } else if NO_WORDS.contains(&cleaned.as_str()) {
        Confirmation::No
    } else {
        Confirmation::Unrelated
    }
}

/// A state-changing tool call held for the user's confirmation.
#[derive(Clone, Debug)]
pub struct PendingToolCall {
    /// The tool to run on confirmation.
    pub name: String,
    /// The arguments the model produced, as its raw JSON string.
    pub arguments: String,
    /// The provider's id for the call, echoed back with its result.
    pub call_id: String,
    /// When the call was held, so a stale one can be dropped.
    pub created: DateTime<Utc>,
}

/// The tool calls awaiting confirmation, one per chat.
///
/// In memory and per chat, the way [`ChatQueues`](crate::queues::ChatQueues) is:
/// a restart forgets an unanswered question, which is the safe direction — a
/// confirmation cannot outlive the process that asked it. The map is guarded by a
/// short-lived `std::sync::Mutex` that is never held across an `await`.
#[derive(Default)]
pub struct PendingConfirmations {
    pending: Mutex<HashMap<ChatId, PendingToolCall>>,
}

impl PendingConfirmations {
    /// An empty set of pending calls.
    pub fn new() -> Self {
        Self::default()
    }

    /// Holds `call` for `chat`, replacing any call already pending there.
    ///
    /// Replacing rather than queueing is deliberate: the newest question is the
    /// one the user is answering, and a chat never has two live questions.
    pub fn put(&self, chat: &ChatId, call: PendingToolCall) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        pending.insert(chat.clone(), call);
    }

    /// Removes and returns the call pending for `chat`, or `None` when there is
    /// none or it is older than `ttl`.
    ///
    /// Taking the call consumes it, so a second message cannot confirm the same
    /// action twice.
    pub fn take(
        &self,
        chat: &ChatId,
        now: DateTime<Utc>,
        ttl: Duration,
    ) -> Option<PendingToolCall> {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let call = pending.remove(chat)?;
        let age = now
            .signed_duration_since(call.created)
            .to_std()
            .unwrap_or_default();
        (age <= ttl).then_some(call)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> AgentConfig {
        AgentConfig::for_test()
    }

    #[test]
    fn an_ordinary_reply_is_passed_through() {
        let system = "You are Megumi. Be brief.";
        assert_eq!(
            screen_reply("  hello there  ", system, &config()).as_deref(),
            Some("hello there")
        );
    }

    #[test]
    fn an_empty_or_no_reply_reply_is_suppressed() {
        let system = "You are Megumi.";
        assert!(screen_reply("", system, &config()).is_none());
        assert!(screen_reply("   ", system, &config()).is_none());
        assert!(screen_reply("NO_REPLY", system, &config()).is_none());
        assert!(screen_reply("no_reply", system, &config()).is_none());
    }

    #[test]
    fn a_reply_carrying_internal_tags_is_suppressed() {
        let system = "You are Megumi.";
        assert!(screen_reply("<memories>secret</memories>", system, &config()).is_none());
        assert!(screen_reply("ok </chat_message> done", system, &config()).is_none());
        assert!(screen_reply("<CONVERSATION_SUMMARY>x", system, &config()).is_none());
    }

    #[test]
    fn a_reply_reciting_the_system_prompt_is_suppressed() {
        let system = "You are Megumi, a helpful assistant in a chat app. Be brief.";
        assert!(
            screen_reply(
                "You are Megumi, a helpful assistant in a chat app.",
                system,
                &config()
            )
            .is_none()
        );
        // A short shared run is fine.
        assert_eq!(
            screen_reply("Megumi here", system, &config()).as_deref(),
            Some("Megumi here")
        );
    }

    #[test]
    fn a_long_reply_is_truncated_to_the_cap() {
        let mut config = config();
        config.max_reply_chars = 5;
        assert_eq!(
            screen_reply("abcdefgh", "s", &config).as_deref(),
            Some("abcde")
        );
    }

    #[test]
    fn confirmations_are_read_from_a_small_word_list() {
        for text in ["yes", "Yes!", "ok", "  sure  ", "go ahead", "DO IT"] {
            assert_eq!(classify_confirmation(text), Confirmation::Yes, "{text:?}");
        }
        for text in ["no", "Nope.", "cancel", "never mind", "stop"] {
            assert_eq!(classify_confirmation(text), Confirmation::No, "{text:?}");
        }
        for text in ["what time is it?", "maybe later", "why?"] {
            assert_eq!(
                classify_confirmation(text),
                Confirmation::Unrelated,
                "{text:?}"
            );
        }
    }

    fn pending(created: DateTime<Utc>) -> PendingToolCall {
        PendingToolCall {
            name: "change_state".into(),
            arguments: "{}".into(),
            call_id: "c1".into(),
            created,
        }
    }

    #[test]
    fn a_pending_call_is_taken_once() {
        let store = PendingConfirmations::new();
        let chat = ChatId::new("gA");
        store.put(&chat, pending(Utc::now()));
        let ttl = Duration::from_secs(300);
        assert!(store.take(&chat, Utc::now(), ttl).is_some());
        // The second take finds nothing: a call cannot be confirmed twice.
        assert!(store.take(&chat, Utc::now(), ttl).is_none());
    }

    #[test]
    fn an_expired_pending_call_is_dropped() {
        let store = PendingConfirmations::new();
        let chat = ChatId::new("gA");
        let long_ago = Utc::now() - chrono::Duration::seconds(10);
        store.put(&chat, pending(long_ago));
        assert!(
            store
                .take(&chat, Utc::now(), Duration::from_secs(5))
                .is_none()
        );
    }
}
