mod audio;
mod client;
pub mod fingerprint;
mod models;
mod signature;
mod source;

use client::ShazamClient;
use megumi::{CreateAttachment, CreateReply, Error, command};

use crate::Context;
use uuid::Uuid;

/// Recognises a song from an audio or video clip.
#[command(name = "shazam", aliases("sz"), react = "📥")]
pub async fn shazam(ctx: Context, #[rest] args: &str) -> Result<(), Error> {
    let source = source::resolve(&ctx, first_word(args)).await?;

    let bytes = match source {
        source::Media::Url(url) => reqwest::get(&url)
            .await
            .map_err(|error| format!("Failed to download: {error}"))?
            .bytes()
            .await
            .map_err(|error| format!("Failed to read bytes: {error}"))?
            .to_vec(),
        source::Media::Bytes(data) => data,
    };

    let uuid = Uuid::new_v4().to_string();
    let input_file = format!("/tmp/{}_in", uuid);
    let output_file = format!("/tmp/{}_out.wav", uuid);

    tokio::fs::write(&input_file, &bytes)
        .await
        .map_err(|error| format!("Failed to save temp file: {error}"))?;

    let _ = ctx.react("⌛").await;

    let ffmpeg = tokio::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-i",
            &input_file,
            "-ar",
            "16000",
            "-ac",
            "1",
            "-c:a",
            "pcm_s16le",
            &output_file,
        ])
        .output()
        .await
        .map_err(|error| format!("Failed to execute ffmpeg: {error}"))?;

    let _ = tokio::fs::remove_file(&input_file).await;

    if !ffmpeg.status.success() {
        return Err("Failed to process audio. Please try a different file.".into());
    }

    let _ = ctx.react("🔎").await;

    let pcm_result = audio::load_wav_as_pcm(&output_file, None).await;
    let _ = tokio::fs::remove_file(&output_file).await;
    let pcm_data = pcm_result.map_err(|error| format!("Failed to decode WAV: {error}"))?;

    let shazam_client =
        ShazamClient::new().map_err(|error| format!("Failed to create client: {error}"))?;

    match shazam_client.recognize_from_pcm(&pcm_data).await {
        Ok(result) => {
            if let Some(track) = result.track {
                send_track_result(&ctx, track).await?;
            } else {
                let _ = ctx.reply_quoting("No match found for this audio.").await;
            }
        }
        Err(e) => {
            return Err(format!("Shazam API error: {}", e).into());
        }
    }

    Ok(())
}

/// The first word typed after the command, which is where a URL goes.
fn first_word(text: &str) -> Option<&str> {
    let word = text.split_whitespace().next()?;
    (!word.is_empty()).then_some(word)
}

async fn send_track_result(ctx: &Context, track: models::Track) -> Result<(), String> {
    let text = format!("🎵 *{}*\n🎤 {}", track.title, track.subtitle);

    let cover = track.images.and_then(|img| {
        if !img.coverarthq.is_empty() {
            Some(img.coverarthq)
        } else if !img.coverart.is_empty() {
            Some(img.coverart)
        } else {
            None
        }
    });

    let _ = ctx.react("✅").await;

    if let Some(cover_url) = cover {
        if let Ok(response) = reqwest::get(&cover_url).await
            && let Ok(bytes) = response.bytes().await
        {
            // Generate a tiny JPEG thumbnail
            let thumbnail = image::load_from_memory(&bytes)
                .ok()
                .map(|img| img.thumbnail(100, 100))
                .and_then(|thumb| {
                    let mut buf = std::io::Cursor::new(Vec::new());
                    thumb.write_to(&mut buf, image::ImageFormat::Jpeg).ok()?;
                    Some(buf.into_inner())
                });

            let attachment = CreateAttachment::image(bytes.to_vec()).thumbnail(thumbnail);
            let sent = ctx
                .send(
                    CreateReply::new()
                        .content(text.as_str())
                        .attachment(attachment)
                        .reply(true),
                )
                .await;
            if sent.is_ok() {
                return Ok(());
            }
        }

        let _ = ctx
            .reply_quoting(format!("{}\n\n{}", text, cover_url))
            .await;
    } else {
        let _ = ctx.reply_quoting(text).await;
    }

    Ok(())
}
