//! The WhatsApp adapter for the agent core.
//!
//! This is the only place that knows both the agent and `whatsapp-rust`. It
//! converts each inbound message into a [`megumi_agent::InboundEvent`] — deciding
//! what a mention or a reply-to-bot is on WhatsApp, and turning a voice note or
//! an image into text via [`media`] — and sends the actions the agent returns.
//! The agent itself never sees a JID.
//!
//! It also holds the WhatsApp-specific identity rule: a person must present the
//! same [`megumi_agent::SenderId`] in their private chat and in every group, or
//! memory and privacy scoping break. WhatsApp addresses one person by either
//! their phone number (PN) or their LID, so the adapter canonicalizes to the
//! phone number when it can and keeps the other form alongside.

pub mod media;

use std::sync::Arc;

use megumi::{MessageContext, MessageExt, parse_command_text};
use tracing::warn;
use whatsapp_rust::prelude::wa;
use whatsapp_rust::types::events::{InboundMessage, MessageBatch};
use whatsapp_rust::types::message::MessageInfo;
use whatsapp_rust::{Client, Jid};

use megumi_agent::{
    Attachment, ChatId, ChatType, InboundEvent, OutboundAction, QuotedMessage, SenderId,
};

use crate::agent::media::OpenAiMedia;
use crate::data::Data;

/// The prefix the command layer answers to. A message carrying it is left to
/// that layer rather than the agent.
const PREFIX: &str = "!";

/// Handles every message in a batch: stores all of them, then runs the turns a
/// trigger calls for.
///
/// Storage happens first and for every message, so history never depends on a
/// model call succeeding. The turns then run in order; each is serialized per
/// chat inside the agent. This runs inline on the event's own task, which
/// WhatsApp's default concurrent delivery spawns per event, so a multi-second
/// turn blocks nothing else — but it must never be combined with
/// `EventDelivery::Ordered`, which drains events through one task and would
/// head-of-line-block the whole client.
///
/// A reply the agent sends comes back as a message from our own account, which
/// the next turn stores with `from_self` set and the context builder renders as
/// the assistant. That echo is how a turn sees its own earlier replies.
pub async fn on_messages(data: &Data, client: &Arc<Client>, batch: &MessageBatch) {
    // The bot's own identities: its phone-number JID and its LID, whichever
    // exist. Both are needed because a mention or a quote carries whichever
    // namespace the sender's client used, and the two are different JIDs.
    let own: Vec<Jid> = [client.pn(), client.lid()].into_iter().flatten().collect();

    let mut events = Vec::with_capacity(batch.len());
    for inbound in batch.iter() {
        let event = inbound_event(inbound, &own, client, data.media.as_ref()).await;
        if let Err(error) = data.agent.ingest(&event) {
            warn!(%error, "the agent could not store a message");
        }
        events.push(event);
    }

    for event in &events {
        match data.agent.respond(event).await {
            Ok(Some(action)) => send(client, action).await,
            Ok(None) => {}
            Err(error) => warn!(%error, chat = %event.chat, "the agent turn failed"),
        }
    }
}

/// Sends one action to WhatsApp.
async fn send(client: &Client, action: OutboundAction) {
    match action {
        OutboundAction::SendText { chat, text } => {
            let Ok(jid) = chat.as_str().parse::<Jid>() else {
                warn!(chat = %chat, "the agent produced an unparseable chat id");
                return;
            };
            if let Err(error) = client.send_text(jid, text).await {
                warn!(%error, chat = %chat, "the agent could not send its reply");
            }
        }
    }
}

/// Converts one inbound message into the agent's shape.
///
/// `media` is the optional provider the adapter describes a voice note or image
/// with; `None` leaves an attachment as its kind and caption alone.
pub async fn inbound_event(
    inbound: &InboundMessage,
    own: &[Jid],
    client: &Client,
    media: Option<&OpenAiMedia>,
) -> InboundEvent {
    event_from_parts(inbound.message.as_ref(), &inbound.info, client, own, media).await
}

/// Converts the message a command was invoked with into the agent's shape.
///
/// A command needs the same event the adapter builds from a live message — the
/// same canonical sender, the same mention and reply flags — so `!ask` can force
/// a turn and `!memory` can build the right reader. It goes through the same
/// [`event_from_parts`] core as [`inbound_event`], so the two agree on the
/// message id: the command stores its event, and the adapter's later ingest of
/// the same message is a no-op.
///
/// Media is not described here: a command's own attachment is not what the
/// command is about, and the description would cost a provider call on every
/// invocation.
pub async fn event_from_context(ctx: &MessageContext) -> InboundEvent {
    let own: Vec<Jid> = [ctx.client.pn(), ctx.client.lid()]
        .into_iter()
        .flatten()
        .collect();
    event_from_parts(&ctx.message, &ctx.info, &ctx.client, &own, None).await
}

/// The platform-specific core both conversions share.
///
/// Takes the message and its metadata rather than a whole inbound message, so a
/// command's [`MessageContext`] and a live [`InboundMessage`] produce an
/// identical event. The message id, chat, and sender must match on both paths,
/// because the command's stored event and the adapter's ingest of the same
/// message are deduplicated by that id.
async fn event_from_parts(
    message: &wa::Message,
    info: &MessageInfo,
    client: &Client,
    own: &[Jid],
    media: Option<&OpenAiMedia>,
) -> InboundEvent {
    let source = &info.source;
    let base = message.get_base_message();
    let text = base.text_content().or_else(|| base.get_caption());

    // The bot's own echoed media is not described: the reply it sent already
    // carried its text, and describing the echo would cost a provider call for
    // nothing. A command's own media is skipped the same way, by passing `None`.
    let media = media.filter(|_| !source.is_from_me);

    let sender = sender_id(client, source).await;
    let sender_alt = source
        .sender_alt
        .as_ref()
        .filter(|jid| jid.to_non_ad_string() != sender.as_str())
        .map(|jid| SenderId::new(jid.to_non_ad_string()));

    InboundEvent {
        message_id: info.id.to_string(),
        chat: ChatId::new(source.chat.to_non_ad_string()),
        chat_type: if source.is_group {
            ChatType::Group
        } else {
            ChatType::Private
        },
        sender,
        sender_alt,
        sender_name: Some(info.push_name.to_string()).filter(|name| !name.is_empty()),
        text: text.map(str::to_string),
        attachments: attachments(base, client, media).await,
        reply_to: quoted_message(base),
        mentions_self: mentions_self(base, own),
        is_reply_to_self: replies_to_self(base, own),
        is_command: text.is_some_and(|text| parse_command_text(text, PREFIX).is_some()),
        from_self: source.is_from_me,
        timestamp: info.timestamp,
    }
}

/// The `ContextInfo` of the base message, whichever sub-message carries it.
///
/// A mention or a quote can ride a caption as easily as a text body, so every
/// sub-message type that can carry one is checked.
fn context_info(message: &wa::Message) -> Option<&wa::ContextInfo> {
    message
        .extended_text_message
        .as_option()
        .and_then(|m| m.context_info.as_option())
        .or_else(|| {
            message
                .image_message
                .as_option()
                .and_then(|m| m.context_info.as_option())
        })
        .or_else(|| {
            message
                .video_message
                .as_option()
                .and_then(|m| m.context_info.as_option())
        })
        .or_else(|| {
            message
                .document_message
                .as_option()
                .and_then(|m| m.context_info.as_option())
        })
}

/// Whether a JID is the bot, by user *and* server.
///
/// `is_same_user_as` alone would be wrong: it ignores the server, so a LID and
/// an unrelated phone number that share a numeric user part would match.
fn is_bot(jid: &Jid, own: &[Jid]) -> bool {
    own.iter().any(|own| jid.is_same_chat_as(own))
}

/// Whether the message @mentions the bot.
fn mentions_self(message: &wa::Message, own: &[Jid]) -> bool {
    context_info(message).is_some_and(|context| {
        context
            .mentioned_jid
            .iter()
            .filter_map(|jid| jid.parse::<Jid>().ok())
            .any(|jid| is_bot(&jid, own))
    })
}

/// Whether the message replies to a message the bot sent.
///
/// The quoted message's sender is in `context_info.participant`.
fn replies_to_self(message: &wa::Message, own: &[Jid]) -> bool {
    context_info(message)
        .and_then(|context| context.participant.as_deref())
        .and_then(|participant| participant.parse::<Jid>().ok())
        .is_some_and(|participant| is_bot(&participant, own))
}

/// The quoted message, as the agent records it.
fn quoted_message(message: &wa::Message) -> Option<QuotedMessage> {
    let context = context_info(message)?;
    let quoted = context.quoted_message.as_option()?;
    let sender = context
        .participant
        .as_deref()
        .map(SenderId::new)
        .unwrap_or_else(|| SenderId::new("unknown"));
    let text = quoted
        .text_content()
        .or_else(|| quoted.get_caption())
        .map(str::to_string);
    Some(QuotedMessage { sender, text })
}

/// The media a message carries, described for the model.
///
/// Only the message's *own* media is read — never a quoted message's — so a text
/// reply to an image does not get the image's description attributed to it. When
/// a provider is configured the audio is transcribed or the image described, and
/// that text is the attachment's `description`. Without a provider, or when the
/// download or the provider fails, the description is `None` and the attachment
/// is just its kind; the caption, if any, already rides in the message text.
async fn attachments(
    message: &wa::Message,
    client: &Client,
    media: Option<&OpenAiMedia>,
) -> Vec<Attachment> {
    let Some(attachment) = megumi::Attachment::own(message) else {
        return Vec::new();
    };
    let kind = match attachment.kind() {
        megumi::MediaKind::Image => "image",
        megumi::MediaKind::Video => "video",
        megumi::MediaKind::Audio => "audio",
        megumi::MediaKind::Document => "document",
        megumi::MediaKind::Sticker => "sticker",
    };

    let description = match media {
        Some(provider) => describe(&attachment, client, provider).await,
        None => None,
    };
    vec![Attachment {
        kind: kind.to_string(),
        description,
    }]
}

/// Transcribes or describes `attachment`, or `None` when it cannot be read.
///
/// A file larger than the provider's cap is refused before the download, and any
/// failure — the download, the request, an unusable reply — is logged and yields
/// `None`, so a media problem never fails the batch.
async fn describe(
    attachment: &megumi::Attachment<'_>,
    client: &Client,
    provider: &OpenAiMedia,
) -> Option<String> {
    // Only audio and images have an endpoint; anything else keeps its kind alone
    // and is not downloaded at all.
    let kind = attachment.kind();
    if !matches!(kind, megumi::MediaKind::Audio | megumi::MediaKind::Image) {
        return None;
    }
    if attachment
        .file_length()
        .is_some_and(|size| size > provider.max_bytes())
    {
        warn!(kind = kind.label(), "the media is too large to describe");
        return None;
    }
    let bytes = match client.download(attachment.downloadable()).await {
        Ok(bytes) => bytes,
        Err(error) => {
            warn!(%error, "the media could not be downloaded");
            return None;
        }
    };
    if bytes.len() as u64 > provider.max_bytes() {
        warn!("the downloaded media is too large to describe");
        return None;
    }
    let mimetype = attachment.mimetype();
    let result = match kind {
        megumi::MediaKind::Audio => provider.transcribe(bytes, mimetype).await,
        megumi::MediaKind::Image => provider.describe_image(bytes, mimetype).await,
        _ => return None,
    };
    match result {
        Ok(text) => Some(text).filter(|text| !text.is_empty()),
        Err(error) => {
            warn!(%error, "the media could not be described");
            None
        }
    }
}

/// The person's stable identity, canonicalized to their phone number.
///
/// Prefers the sender's phone-number form, then its alternate, then a LID-to-PN
/// lookup. Falls back to whatever form is present. The caveat: the LID-to-PN
/// mapping is learned asynchronously, so a person first seen as a LID before the
/// mapping is known gets a LID-keyed id until reconciliation — which is why the
/// alternate form is kept on the event.
async fn sender_id(
    client: &Client,
    source: &whatsapp_rust::types::message::MessageSource,
) -> SenderId {
    if source.sender.is_pn() {
        return SenderId::new(source.sender.to_non_ad_string());
    }
    if let Some(alt) = source.sender_alt.as_ref().filter(|jid| jid.is_pn()) {
        return SenderId::new(alt.to_non_ad_string());
    }
    if source.sender.is_lid()
        && let Ok(Some(entry)) = client.get_lid_pn_entry(&source.sender).await
    {
        return SenderId::new(format!("{}@s.whatsapp.net", entry.phone_number));
    }
    SenderId::new(source.sender.to_non_ad_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use whatsapp_rust::buffa::MessageField;

    /// A text message carrying `context`, for the mention/reply checks.
    fn text_with_context(text: &str, context: wa::ContextInfo) -> wa::Message {
        wa::Message {
            extended_text_message: MessageField::some(wa::message::ExtendedTextMessage {
                text: Some(text.to_string()),
                context_info: MessageField::some(context),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn a_bot_mention_is_detected_by_user_and_server() {
        let own: Vec<Jid> = vec!["62812@s.whatsapp.net".parse().unwrap()];
        let context = wa::ContextInfo {
            mentioned_jid: vec!["62812@s.whatsapp.net".to_string()],
            ..Default::default()
        };
        assert!(mentions_self(&text_with_context("@bot hi", context), &own));
    }

    #[test]
    fn a_lid_does_not_match_a_phone_number_with_the_same_user() {
        let own: Vec<Jid> = vec!["62812@s.whatsapp.net".parse().unwrap()];
        let context = wa::ContextInfo {
            mentioned_jid: vec!["62812@lid".to_string()],
            ..Default::default()
        };
        assert!(!mentions_self(&text_with_context("hi", context), &own));
    }

    #[test]
    fn a_reply_to_a_bot_message_is_detected_from_the_participant() {
        let own: Vec<Jid> = vec!["62812@s.whatsapp.net".parse().unwrap()];
        let context = wa::ContextInfo {
            participant: Some("62812@s.whatsapp.net".to_string()),
            ..Default::default()
        };
        assert!(replies_to_self(
            &text_with_context("what did you mean?", context),
            &own
        ));
    }

    #[test]
    fn a_caption_mention_on_an_image_is_detected() {
        let own: Vec<Jid> = vec!["62812@s.whatsapp.net".parse().unwrap()];
        let context = wa::ContextInfo {
            mentioned_jid: vec!["62812@s.whatsapp.net".to_string()],
            ..Default::default()
        };
        let message = wa::Message {
            image_message: MessageField::some(wa::message::ImageMessage {
                caption: Some("@bot what is this?".to_string()),
                context_info: MessageField::some(context),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(mentions_self(&message, &own));
    }

    #[test]
    fn a_command_is_recognized() {
        assert!(parse_command_text("!ping", PREFIX).is_some());
        assert!(parse_command_text("  !group info", PREFIX).is_some());
        assert!(parse_command_text("hello", PREFIX).is_none());
        assert!(parse_command_text("just a ! in text", PREFIX).is_none());
    }
}
