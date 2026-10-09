//! The platform boundary: the message shape the agent consumes and the action
//! shape it produces.
//!
//! An adapter owns everything platform-specific. It converts its own message
//! type into an [`InboundEvent`] — deciding what a "mention" or a "reply to the
//! bot" is on that platform — and executes the [`OutboundAction`]s the agent
//! returns. The agent never parses a chat platform's identifiers, so
//! [`ChatId`] and [`SenderId`] are opaque strings to it.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The kind of conversation a message belongs to.
///
/// The mode decides how the agent behaves: a group needs an explicit trigger
/// before the agent speaks, while a private chat is answered by default.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChatType {
    /// A group chat, where several people speak.
    Group,
    /// A one-on-one chat.
    Private,
}

/// A conversation identifier, opaque to the agent.
///
/// The adapter supplies whatever string identifies the chat on its platform.
/// The agent uses it only as a map key, so it never has to understand its
/// shape.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChatId(String);

impl ChatId {
    /// Wraps `id`, which the adapter chose.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The identifier as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ChatId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A stable person identifier, opaque to the agent.
///
/// The same person must present the same [`SenderId`] across their private chat
/// and every group they are in, or memory and privacy scoping break. Producing
/// that is the adapter's job; the agent treats the value as an opaque key and
/// never links identities by display name.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SenderId(String);

impl SenderId {
    /// Wraps `id`, which the adapter chose.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The identifier as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SenderId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The media a message carries, already described by the adapter.
///
/// The agent does not fetch or decode media itself; an adapter that can turn a
/// voice note into text does so before building the event, so the model sees a
/// description rather than a pointer it cannot resolve. `description` is that
/// adapter-produced rendering — a transcript or an image description — and is
/// `None` when the adapter could not produce one. A caption is not stored here:
/// the adapter puts it in [`InboundEvent::text`], so carrying it twice would
/// duplicate it in the prompt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    /// What kind of media this is, for the model's benefit.
    pub kind: String,
    /// A text rendering of the media — a transcript or a description — when the
    /// adapter could produce one.
    pub description: Option<String>,
}

/// The message an inbound message quotes, if any.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QuotedMessage {
    /// The quoted message's author, as a [`SenderId`].
    pub sender: SenderId,
    /// The quoted message's text, when it had any.
    pub text: Option<String>,
}

/// One inbound message, normalized to the agent's platform-agnostic shape.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InboundEvent {
    /// The platform's message id, used to make processing idempotent.
    pub message_id: String,
    /// The conversation the message arrived in.
    pub chat: ChatId,
    /// Whether the conversation is a group or a private chat.
    pub chat_type: ChatType,
    /// The author's stable identity.
    pub sender: SenderId,
    /// The author's alternate identity, when the platform knows a second one.
    ///
    /// WhatsApp addresses one person by either their phone number or their LID,
    /// and the adapter may hold both. Keeping the pair lets identity
    /// reconciliation happen later; the agent only stores it.
    pub sender_alt: Option<SenderId>,
    /// The author's display name. Presentation only — never an identity.
    pub sender_name: Option<String>,
    /// The message text, when it had any.
    pub text: Option<String>,
    /// Media the adapter already described.
    pub attachments: Vec<Attachment>,
    /// The message this one quotes, if any.
    pub reply_to: Option<QuotedMessage>,
    /// Whether the message mentions the agent's own account.
    pub mentions_self: bool,
    /// Whether the message replies to a message the agent sent.
    pub is_reply_to_self: bool,
    /// Whether the message is one of the bot's own commands.
    ///
    /// A command is answered by the command layer, not the agent, so the agent
    /// stays silent on it even when it also mentions the bot.
    pub is_command: bool,
    /// Whether the message is one the agent itself sent, echoed back.
    pub from_self: bool,
    /// When the platform says the message was sent.
    pub timestamp: DateTime<Utc>,
}

impl InboundEvent {
    /// The text with any mention markers the adapter left in it trimmed, or an
    /// empty string when the message had no text.
    ///
    /// A bare mention leaves the adapter with nothing to strip, so this is the
    /// trigger message as the model should see it.
    pub fn trimmed_text(&self) -> &str {
        self.text.as_deref().unwrap_or("").trim()
    }
}

/// Something the agent wants the adapter to do in response to a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutboundAction {
    /// Send `text` to `chat`.
    SendText {
        /// The conversation to send to.
        chat: ChatId,
        /// The message body.
        text: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trimmed_text_treats_a_bare_mention_as_empty() {
        let event = |text: Option<&str>| InboundEvent {
            message_id: "m".into(),
            chat: ChatId::new("c"),
            chat_type: ChatType::Group,
            sender: SenderId::new("u"),
            sender_alt: None,
            sender_name: None,
            text: text.map(str::to_string),
            attachments: Vec::new(),
            reply_to: None,
            mentions_self: true,
            is_reply_to_self: false,
            is_command: false,
            from_self: false,
            timestamp: Utc::now(),
        };
        assert_eq!(event(Some("  hi  ")).trimmed_text(), "hi");
        assert_eq!(event(Some("   ")).trimmed_text(), "");
        assert_eq!(event(None).trimmed_text(), "");
    }
}
