//! `!download` fetches a video from a public address with `yt-dlp`.

pub mod ytdlp;

use megumi::{CreateAttachment, CreateReply, Error, command};

use crate::Context;

/// The largest file sent as a playable video. Past this, WhatsApp rejects the
/// upload outright, so the file goes out as a document instead — those are
/// accepted up to a much higher limit and the recipient downloads them.
const VIDEO_LIMIT: u64 = 64 * 1024 * 1024;

/// Downloads a video from a URL.
///
/// Needs [`yt-dlp`](https://github.com/yt-dlp/yt-dlp) on `PATH`.
#[command(name = "download", aliases("dl", "ytdl"), react = "⌛")]
pub async fn download(ctx: Context, #[rest] args: &str) -> Result<(), Error> {
    let url = ytdlp::address(ytdlp::first_word(args)).ok_or(
        "Send `!download <url>` with an http or https address.\n\
         Example: !download https://www.youtube.com/watch?v=dQw4w9WgXcQ",
    )?;

    let _ = ctx.react("📥").await;

    let video = ytdlp::download(&url)
        .await
        .map_err(|error| error.to_string())?;

    let _ = ctx.react("📤").await;

    let caption = video.caption();
    let file_name = video.file_name();
    let attachment = if video.bytes.len() as u64 <= VIDEO_LIMIT {
        let mut attachment = CreateAttachment::video(video.bytes);
        if let Some(seconds) = video.seconds {
            attachment = attachment.seconds(seconds);
        }
        if let Some((width, height)) = video.size {
            attachment = attachment.size(width, height);
        }
        attachment
    } else {
        CreateAttachment::document(video.bytes, file_name)
    }
    .thumbnail(video.thumbnail);

    ctx.send(
        CreateReply::new()
            .content(caption)
            .attachment(attachment)
            .reply(true),
    )
    .await?;

    Ok(())
}
