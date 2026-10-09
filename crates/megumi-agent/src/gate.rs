//! Deciding whether a message is answered.
//!
//! The agent is trigger-based, not proactive: it never decides on its own to
//! speak. A group message must mention the agent, reply to it, or be a command
//! (which the command layer handles, so the agent stays silent); a private
//! message is answered unless it is a bare acknowledgement. The decision is a
//! pure function of the event, so it is exhaustively testable with no client and
//! no clock.

use crate::event::{ChatType, InboundEvent};

/// Why the agent decided to speak.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// The message @mentions the agent.
    Mention,
    /// The message replies to a message the agent sent.
    Reply,
    /// A private-chat message that is not a bare acknowledgement.
    PrivateMessage,
    /// The message answers a tool confirmation the agent asked for.
    ///
    /// Not produced by [`decide`]: it is the reason the agent spoke when it
    /// resolved a held tool call, so it appears only in a turn's trace.
    Confirmation,
    /// A command forced the agent to speak.
    ///
    /// Not produced by [`decide`]: a command is left to the command layer, so
    /// [`decide`] stays silent on it. The command router runs the turn itself
    /// and labels it with this, so the trace distinguishes a command-driven
    /// turn from a mention or a DM.
    Command,
}

/// Why the agent stayed silent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SilenceReason {
    /// The message is the agent's own, echoed back.
    FromSelf,
    /// A command, which the command layer answers.
    Command,
    /// A group message with no mention and no reply to the agent.
    NoTrigger,
    /// A message with no text and nothing to answer.
    Empty,
    /// A private-chat message that only acknowledges.
    Acknowledgement,
}

/// The gate's answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateDecision {
    /// Speak, for the given reason.
    Respond(Trigger),
    /// Stay silent, for the given reason.
    StaySilent(SilenceReason),
}

/// Decides whether `event` should be answered.
///
/// A message the agent sent itself and a command are silent in every chat: the
/// former is an echo, the latter belongs to the command layer, and answering
/// either would double up. Everything else follows the mode.
pub fn decide(event: &InboundEvent) -> GateDecision {
    use GateDecision::{Respond, StaySilent};

    if event.from_self {
        return StaySilent(SilenceReason::FromSelf);
    }
    if event.is_command {
        return StaySilent(SilenceReason::Command);
    }

    match event.chat_type {
        ChatType::Group => {
            if event.mentions_self {
                Respond(Trigger::Mention)
            } else if event.is_reply_to_self {
                Respond(Trigger::Reply)
            } else {
                StaySilent(SilenceReason::NoTrigger)
            }
        }
        ChatType::Private => {
            // A message the adapter understood is answered even with no text of
            // its own: a voice note or image the agent can read is a question,
            // not a bare attachment. An attachment the adapter could not
            // describe is not — it falls through to the empty/acknowledgement
            // checks, exactly as before media understanding existed.
            let understood = event
                .attachments
                .iter()
                .any(|attachment| attachment.description.is_some());
            if understood {
                Respond(Trigger::PrivateMessage)
            } else if event.trimmed_text().is_empty() && event.attachments.is_empty() {
                StaySilent(SilenceReason::Empty)
            } else if is_acknowledgement(event.trimmed_text()) {
                StaySilent(SilenceReason::Acknowledgement)
            } else {
                Respond(Trigger::PrivateMessage)
            }
        }
    }
}

/// Words that acknowledge without asking for anything.
///
/// Kept deliberately small and matched after lowercasing and trimming
/// punctuation: a false "acknowledgement" would ignore a real question, so only
/// clear cases qualify. A message with no letters or digits at all — a lone
/// emoji, a thumbs-up, "..." — counts too.
fn is_acknowledgement(text: &str) -> bool {
    const WORDS: &[&str] = &[
        "ok",
        "okay",
        "k",
        "kk",
        "sure",
        "cool",
        "nice",
        "thanks",
        "thank you",
        "thx",
        "ty",
        "tq",
        "lol",
        "lmao",
        "haha",
        "hehe",
        "hmm",
        "hm",
        "ya",
        "yep",
        "yes",
        "no",
        "nope",
        "nah",
        "wkwk",
        "wkwkwk",
    ];
    let cleaned = text
        .trim()
        .to_lowercase()
        .trim_matches(|ch: char| ch.is_whitespace() || ch.is_ascii_punctuation())
        .to_string();
    WORDS.contains(&cleaned.as_str()) || !text.chars().any(char::is_alphanumeric)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{ChatId, SenderId};
    use chrono::Utc;

    fn event(chat_type: ChatType, text: Option<&str>) -> InboundEvent {
        InboundEvent {
            message_id: "m".into(),
            chat: ChatId::new("c"),
            chat_type,
            sender: SenderId::new("u"),
            sender_alt: None,
            sender_name: None,
            text: text.map(str::to_string),
            attachments: Vec::new(),
            reply_to: None,
            mentions_self: false,
            is_reply_to_self: false,
            is_command: false,
            from_self: false,
            timestamp: Utc::now(),
        }
    }

    #[test]
    fn a_group_needs_a_mention_or_a_reply() {
        assert_eq!(
            decide(&event(ChatType::Group, Some("hello"))),
            GateDecision::StaySilent(SilenceReason::NoTrigger)
        );

        let mut mentioned = event(ChatType::Group, Some("hello"));
        mentioned.mentions_self = true;
        assert_eq!(decide(&mentioned), GateDecision::Respond(Trigger::Mention));

        let mut replied = event(ChatType::Group, Some("hello"));
        replied.is_reply_to_self = true;
        assert_eq!(decide(&replied), GateDecision::Respond(Trigger::Reply));
    }

    #[test]
    fn a_command_is_left_to_the_command_layer() {
        let mut command = event(ChatType::Group, Some("!ping"));
        command.is_command = true;
        command.mentions_self = true;
        assert_eq!(
            decide(&command),
            GateDecision::StaySilent(SilenceReason::Command)
        );
    }

    #[test]
    fn an_echo_of_our_own_message_is_ignored() {
        let mut own = event(ChatType::Private, Some("hi"));
        own.from_self = true;
        assert_eq!(
            decide(&own),
            GateDecision::StaySilent(SilenceReason::FromSelf)
        );
    }

    #[test]
    fn a_private_chat_answers_every_real_message() {
        assert_eq!(
            decide(&event(
                ChatType::Private,
                Some("what is the capital of France?")
            )),
            GateDecision::Respond(Trigger::PrivateMessage)
        );
    }

    #[test]
    fn a_private_acknowledgement_is_skipped() {
        for text in ["ok", "Ok!", "👍", "thanks", "  thx  ", "..."] {
            assert_eq!(
                decide(&event(ChatType::Private, Some(text))),
                GateDecision::StaySilent(SilenceReason::Acknowledgement),
                "{text:?} should be an acknowledgement"
            );
        }
    }

    #[test]
    fn a_private_message_with_no_text_is_skipped() {
        assert_eq!(
            decide(&event(ChatType::Private, None)),
            GateDecision::StaySilent(SilenceReason::Empty)
        );
        assert_eq!(
            decide(&event(ChatType::Private, Some("   "))),
            GateDecision::StaySilent(SilenceReason::Empty)
        );
    }

    #[test]
    fn a_private_message_with_only_an_understood_attachment_is_answered() {
        let mut described = event(ChatType::Private, None);
        described.attachments = vec![crate::event::Attachment {
            kind: "audio".into(),
            description: Some("what time is the meeting?".into()),
        }];
        assert_eq!(
            decide(&described),
            GateDecision::Respond(Trigger::PrivateMessage)
        );
    }

    #[test]
    fn a_private_message_with_an_undescribed_attachment_stays_silent() {
        // Without a media provider a bare voice note is still nothing to answer.
        let mut bare = event(ChatType::Private, None);
        bare.attachments = vec![crate::event::Attachment {
            kind: "sticker".into(),
            description: None,
        }];
        assert!(matches!(decide(&bare), GateDecision::StaySilent(_)));
    }
}
