//! Finding the media a shazam command is made from.

use megumi::MediaKind;

use crate::Context;

const USAGE: &str = "Reply to an audio or video with `!shazam`, or send `!shazam <url>`.";

/// The media kinds this command recognises.
const RECOGNISABLE: [MediaKind; 2] = [MediaKind::Audio, MediaKind::Video];

/// The largest media the recogniser is asked to work on.
const MAX_MEDIA_BYTES: u64 = 50 * 1024 * 1024;

/// Media on its way to the recogniser.
pub enum Media {
    /// A public address the service fetches itself.
    Url(String),
    /// Bytes downloaded from the chat.
    Bytes(Vec<u8>),
}

/// The media to recognize: the address given as an argument, else the media this
/// message carries, else the media of the message it replies to.
pub async fn resolve(ctx: &Context, argument: Option<&str>) -> Result<Media, String> {
    if let Some(argument) = argument {
        return match address(argument) {
            Some(url) => Ok(Media::Url(url)),
            None => Err(format!("`{argument}` is not an http or https address.")),
        };
    }

    let Some(attachment) = ctx
        .attachment()
        .filter(|attachment| RECOGNISABLE.contains(&attachment.kind()))
    else {
        return Err(USAGE.to_string());
    };

    let what = attachment.kind().label();

    // Refusing before the transfer what the recogniser would refuse after it.
    if attachment
        .file_length()
        .is_some_and(|length| length > MAX_MEDIA_BYTES)
    {
        return Err(format!(
            "That {what} is above the {} MB conversion limit.",
            MAX_MEDIA_BYTES / (1024 * 1024)
        ));
    }

    ctx.download(&attachment)
        .await
        .map(Media::Bytes)
        .map_err(|error| format!("Could not download that {what}: {error}"))
}

/// Only the schemes the conversion service is willing to fetch from.
fn address(argument: &str) -> Option<String> {
    let argument = argument.trim();
    (argument.starts_with("http://") || argument.starts_with("https://"))
        .then(|| argument.to_string())
}
