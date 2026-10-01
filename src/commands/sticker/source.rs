//! Finding the media a sticker is made from.

use megumi::MediaKind;

use crate::Context;

const USAGE: &str =
    "Reply to an image, video, or sticker with `!sticker`, or send `!sticker <url>`.";

/// The media kinds this command converts.
const CONVERTIBLE: [MediaKind; 3] = [MediaKind::Image, MediaKind::Video, MediaKind::Sticker];

/// Media a sticker is made from.
pub enum Media {
    /// A public address ffmpeg fetches itself.
    Url(String),
    /// Bytes downloaded from the chat, and the kind they arrived as.
    Bytes { kind: MediaKind, data: Vec<u8> },
}

/// Where the media comes from, before any download.
///
/// A URL argument always wins. Any other argument is leftover words and is
/// ignored when convertible media is already on the message (or quoted), so
/// `!sticker please` on an image still converts the image.
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Url(String),
    Attachment,
}

/// Picks the source: a URL argument always wins; leftover words do not prevent
/// converting media that is already on the message.
pub fn decide(argument: Option<&str>, has_convertible_media: bool) -> Result<Decision, String> {
    if let Some(url) = argument.and_then(address) {
        return Ok(Decision::Url(url));
    }
    if has_convertible_media {
        return Ok(Decision::Attachment);
    }
    Err(match argument {
        Some(argument) => format!("`{argument}` is not an http or https address."),
        None => USAGE.to_string(),
    })
}

/// The media to convert: the address given as an argument, else the media this
/// message carries, else the media of the message it replies to.
pub async fn resolve(ctx: &Context, argument: Option<&str>) -> Result<Media, String> {
    let attachment = ctx
        .attachment()
        .filter(|attachment| CONVERTIBLE.contains(&attachment.kind()));

    match decide(argument, attachment.is_some())? {
        Decision::Url(url) => Ok(Media::Url(url)),
        Decision::Attachment => {
            let Some(attachment) = attachment else {
                return Err(USAGE.to_string());
            };

            let what = attachment.kind().label();
            let data = ctx
                .download(&attachment)
                .await
                .map_err(|error| format!("Could not download that {what}: {error}"))?;

            Ok(Media::Bytes {
                kind: attachment.kind(),
                data,
            })
        }
    }
}

/// Only http/https addresses are accepted.
fn address(argument: &str) -> Option<String> {
    let argument = argument.trim();
    (argument.starts_with("http://") || argument.starts_with("https://"))
        .then(|| argument.to_string())
}
