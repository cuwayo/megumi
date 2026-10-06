//! `!sticker` turns media into a WhatsApp sticker, or converts a Telegram sticker pack.

pub mod lottie;
pub mod source;
pub mod telegram;
pub mod transcode;

use megumi::{CreateAttachment, CreateReply, Error, MediaKind, MessageExt, command};

use crate::Context;
use whatsapp_rust::http::{HttpClient, HttpRequest};
use whatsapp_rust::prelude::{MessageField, wa};

/// Turns media into a sticker, or converts a Telegram sticker pack.
///
/// Reply to a message with media, or pass a URL.
#[command(
    name = "sticker",
    aliases("stiker", "stickerpack", "stikerpack"),
    react = "⌛"
)]
async fn sticker(ctx: Context, #[rest] args: &str) -> Result<(), Error> {
    // Check if user is asking to convert a Telegram sticker pack
    if let Some((pack_slug, custom_name)) = resolve_telegram_pack(&ctx, args) {
        let messages =
            telegram::convert_telegram_pack(&ctx.message.client, &pack_slug, custom_name)
                .await
                .map_err(|error| error.to_string())?;

        // The command answers the message it was invoked with, packs included.
        let quote = ctx.message.build_quote_context();
        for mut message in messages {
            if let Some(pack) = message.sticker_pack_message.as_option_mut() {
                pack.context_info = MessageField::some(quote.clone());
            }
            ctx.send_raw(message).await?;
        }

        return Ok(());
    }

    let source = source::resolve(&ctx, first_word(args)).await?;
    let (sticker, animated) = convert(ctx.message.client.http_client.as_ref(), source).await?;

    ctx.send(
        CreateReply::new()
            .attachment(
                CreateAttachment::sticker(sticker, animated).size(transcode::SIDE, transcode::SIDE),
            )
            .reply(true),
    )
    .await?;

    Ok(())
}

/// The finished sticker and whether WhatsApp should treat it as animated.
///
/// All media is converted locally by ffmpeg; no external conversion service is
/// used, which keeps every request on the machine and eliminates the round-trip
/// to a third-party endpoint.
async fn convert(http: &dyn HttpClient, source: source::Media) -> Result<(Vec<u8>, bool), String> {
    match source {
        source::Media::Url(url) => {
            let response = http
                .execute(HttpRequest::get(&url))
                .await
                .map_err(|error| format!("Could not fetch that url: {error}"))?;
            if response.status_code != 200 {
                return Err(format!(
                    "Could not fetch that url (HTTP {}).",
                    response.status_code
                ));
            }
            let data = response.body;
            let animated = transcode::is_animated(&data).await;
            Ok((transcode::to_sticker(&data, animated).await?, animated))
        }
        source::Media::Bytes { kind, data } => {
            // A video moves whatever container it arrived in; anything else only
            // does if its bytes or an ffprobe frame count say so.
            let animated = kind == MediaKind::Video || transcode::is_animated(&data).await;
            let sticker = transcode::to_sticker(&data, animated).await?;
            Ok((sticker, animated))
        }
    }
}

/// Resolves a Telegram sticker pack slug and optional custom pack name from the
/// argument text or a quoted message.
fn resolve_telegram_pack(ctx: &Context, args: &str) -> Option<(String, Option<String>)> {
    let (first, rest) = split_first_word(args);

    if let Some(first) = first
        && let Some(slug) = telegram::extract_pack_name(first)
    {
        return Some((slug.to_string(), pack_name(rest)));
    }

    let message = ctx.message.message.get_base_message();
    if let Some(text) = quoted_text(message)
        && let Some(slug) = telegram::extract_pack_name(&text)
    {
        return Some((slug.to_string(), pack_name(args)));
    }

    None
}

/// The first whitespace-separated word and everything after it.
fn split_first_word(text: &str) -> (Option<&str>, &str) {
    let text = text.trim_start();
    let Some(end) = text.find(char::is_whitespace) else {
        return ((!text.is_empty()).then_some(text), "");
    };
    (Some(&text[..end]), text[end..].trim_start())
}

fn first_word(text: &str) -> Option<&str> {
    split_first_word(text).0
}

/// A pack name typed after the slug, when one was.
fn pack_name(text: &str) -> Option<String> {
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// Extracts text from a quoted message if present.
fn quoted_text(message: &wa::Message) -> Option<String> {
    let quoted = message
        .extended_text_message
        .as_option()
        .and_then(|extended| extended.context_info.as_option())
        .and_then(|context| context.quoted_message.as_option())?;

    if let Some(conv) = &quoted.conversation {
        return Some(conv.clone());
    }
    if let Some(ext) = quoted.extended_text_message.as_option()
        && let Some(text) = &ext.text
    {
        return Some(text.clone());
    }
    None
}
