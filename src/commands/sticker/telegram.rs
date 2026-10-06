//! Fetching and converting Telegram sticker packs into WhatsApp sticker packs.

use std::io::Cursor;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use image::{DynamicImage, ImageFormat, Rgb, RgbImage};
use serde::Deserialize;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use whatsapp_rust::Client;
use whatsapp_rust::anyhow::{Error, Result, anyhow, bail};
use whatsapp_rust::download::MediaType;
use whatsapp_rust::prelude::wa;
use whatsapp_rust::sticker_pack::{
    StickerInput, StickerPackMetadata, build_sticker_pack_message, create_sticker_pack_zip,
};
use whatsapp_rust::upload::UploadOptions;

use super::transcode;

/// WhatsApp's maximum number of stickers per pack.
pub const MAX_STICKERS_PER_PACK: usize = 60;

/// How long one Telegram request may take before it is given up on.
const TIMEOUT: Duration = Duration::from_secs(30);

/// Telegram drops and throttles requests; one that fails that way is worth
/// repeating, waiting twice as long before each further attempt.
pub const ATTEMPTS: u32 = 4;
const BACKOFF: Duration = Duration::from_millis(300);

#[derive(Debug, Deserialize)]
struct TgResponse<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct TgStickerSet {
    pub name: String,
    pub title: String,
    #[serde(default)]
    pub is_animated: bool,
    #[serde(default)]
    pub is_video: bool,
    pub stickers: Vec<TgSticker>,
    pub thumbnail: Option<TgPhotoSize>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct TgSticker {
    pub file_id: String,
    pub file_unique_id: String,
    pub width: u32,
    pub height: u32,
    #[serde(default)]
    pub is_animated: bool,
    #[serde(default)]
    pub is_video: bool,
    pub emoji: Option<String>,
    pub file_size: Option<u64>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub struct TgPhotoSize {
    pub file_id: String,
    pub width: u32,
    pub height: u32,
    pub file_size: Option<u64>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct TgFile {
    pub file_id: String,
    pub file_path: Option<String>,
}

/// Extracts a Telegram sticker pack name/slug from a URL or raw set name.
pub fn extract_pack_name(input: &str) -> Option<&str> {
    let input = input.trim();
    if let Some(pos) = input.find("t.me/addstickers/") {
        let rest = &input[pos + "t.me/addstickers/".len()..];
        let name = rest.split(['?', '/', '#', '&', ' ', '\n', '\r']).next()?;
        if !name.is_empty() {
            return Some(name);
        }
    }
    if let Some(pos) = input.find("telegram.me/addstickers/") {
        let rest = &input[pos + "telegram.me/addstickers/".len()..];
        let name = rest.split(['?', '/', '#', '&', ' ', '\n', '\r']).next()?;
        if !name.is_empty() {
            return Some(name);
        }
    }
    if let Some(pos) = input.find("tg://addstickers?set=") {
        let rest = &input[pos + "tg://addstickers?set=".len()..];
        let name = rest.split(['?', '/', '#', '&', ' ', '\n', '\r']).next()?;
        if !name.is_empty() {
            return Some(name);
        }
    }
    None
}

fn telegram_api_base() -> String {
    std::env::var("TELEGRAM_API_BASE")
        .unwrap_or_else(|_| "https://api.telegram.org".to_string())
        .trim_end_matches('/')
        .to_string()
}

/// The HTTP client every fetch shares, so the connection and TLS session opened
/// for one request is still there for the next sticker: a pack needs a request
/// per sticker, and a fresh handshake for each would be most of the transfer.
fn http_client() -> Result<&'static reqwest::Client> {
    static CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();

    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .timeout(TIMEOUT)
                .build()
                .map_err(|error| format!("Failed to initialise the HTTP client: {error}"))
        })
        .as_ref()
        .map_err(|error| anyhow!("{error}"))
}

/// The bot token Telegram authenticates every request with, if it is set at all.
fn telegram_token() -> Result<String> {
    std::env::var("TELEGRAM_BOT_TOKEN")
        .ok()
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "Telegram Bot Token is not set.\n\
                 Please add `TELEGRAM_BOT_TOKEN=your_token` into your `.env` file."
            )
        })
}

/// One answered Telegram request: its status and its body, kept together so the
/// caller can still read Telegram's own words when the request was refused.
struct Answer {
    status: reqwest::StatusCode,
    body: Vec<u8>,
}

/// GETs `url` and answers with what Telegram sent back, repeating a request the
/// network or Telegram itself dropped until the attempts run out.
///
/// The bot token travels inside every Telegram url, so a failure must not report
/// the url that carried it: both errors below are stripped of it first.
async fn fetch(http: &reqwest::Client, url: &str, what: &str) -> Result<Answer> {
    let mut wait = BACKOFF;
    let mut attempt = 1;

    loop {
        let last = attempt == ATTEMPTS;

        match http.get(url).send().await {
            Ok(response) => {
                let status = response.status();
                if status.is_success() || last || !retryable(status) {
                    return response
                        .bytes()
                        .await
                        .map(|body| Answer {
                            status,
                            body: body.to_vec(),
                        })
                        .map_err(|error| anyhow!("{what}: {}", brief(error)));
                }
            }
            Err(error) if !last && retryable_error(&error) => {}
            Err(error) => bail!("{what}: {}", brief(error)),
        }

        tokio::time::sleep(wait).await;
        wait *= 2;
        attempt += 1;
    }
}

/// Whether this answer says the same request is worth repeating: Telegram
/// throttled it, or it failed while serving it.
fn retryable(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// Whether this transport failure is worth repeating. A request that never left
/// can succeed on the next attempt; one that was never going to be sent cannot.
fn retryable_error(error: &reqwest::Error) -> bool {
    !error.is_builder() && !error.is_redirect()
}

/// What went wrong, without the url that names the bot token.
fn brief(error: reqwest::Error) -> String {
    error.without_url().to_string()
}

/// Telegram's own words for why it refused a request, or a stand-in when it
/// sent none.
fn refusal(description: Option<String>) -> String {
    description.unwrap_or_else(|| "unknown error".into())
}

/// Fetches a Telegram sticker set by its short name.
pub async fn fetch_sticker_set(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    name: &str,
) -> Result<TgStickerSet> {
    let url = format!("{base}/bot{token}/getStickerSet?name={name}");
    let answer = fetch(http, &url, "Failed to query the Telegram Bot API").await?;

    let response: TgResponse<TgStickerSet> = match serde_json::from_slice(&answer.body) {
        Ok(response) => response,
        Err(error) if answer.status.is_success() => {
            bail!("Failed to parse Telegram response: {error}")
        }
        Err(_) => bail!(
            "Failed to query the Telegram Bot API (HTTP {})",
            answer.status
        ),
    };

    if !response.ok {
        bail!(
            "Telegram rejected request: {}",
            refusal(response.description)
        );
    }

    response
        .result
        .ok_or_else(|| anyhow!("Telegram returned empty result for sticker set `{name}`"))
}

/// Gets the direct download path for a file ID in Telegram.
async fn get_file_path(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    file_id: &str,
) -> Result<String> {
    let url = format!("{base}/bot{token}/getFile?file_id={file_id}");
    let answer = fetch(http, &url, "Failed to get file info from Telegram").await?;

    let response: TgResponse<TgFile> = match serde_json::from_slice(&answer.body) {
        Ok(response) => response,
        Err(error) if answer.status.is_success() => {
            bail!("Failed to parse getFile response: {error}")
        }
        Err(_) => bail!(
            "Failed to get file info from Telegram (HTTP {})",
            answer.status
        ),
    };

    if !response.ok {
        bail!("Telegram getFile error: {}", refusal(response.description));
    }

    let file = response
        .result
        .ok_or_else(|| anyhow!("No file result returned from Telegram"))?;

    file.file_path
        .ok_or_else(|| anyhow!("No file_path returned for file_id {file_id}"))
}

/// Downloads raw file bytes from Telegram CDN.
async fn download_file(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    file_path: &str,
) -> Result<Vec<u8>> {
    let url = format!("{base}/file/bot{token}/{file_path}");
    let answer = fetch(http, &url, &format!("Failed to download `{file_path}`")).await?;

    if !answer.status.is_success() {
        bail!("Failed to download `{file_path}` (HTTP {})", answer.status);
    }

    Ok(answer.body)
}

/// Prepares a 512x512 WebP sticker from raw media data.
///
/// Telegram serves static stickers as conforming WebP files, Lottie stickers as
/// gzipped `.tgs` files, and video stickers as `.webm`. A gzip signature takes
/// the Lottie renderer regardless of Telegram metadata, which keeps mixed packs
/// and incorrectly marked media from being sent through ffmpeg unreadable.
///
/// Telegram's video flag is retained as a fallback for media whose container
/// does not identify its animation by itself.
async fn prepare_sticker_data(data: &[u8], is_video: bool) -> Result<Vec<u8>> {
    if data.starts_with(&[0x1f, 0x8b]) {
        return super::lottie::to_sticker(data).await;
    }
    let animated = is_video || transcode::is_animated(data).await;
    transcode::to_sticker(data, animated)
        .await
        .map_err(|error| anyhow!("Failed to prepare sticker: {error}"))
}

/// Creates a 512x512 JPEG thumbnail from a WebP cover image with a white background.
pub fn create_jpeg_thumbnail(cover_webp: &[u8]) -> Result<Vec<u8>> {
    let dynamic_img = image::load_from_memory(cover_webp)
        .map_err(|error| anyhow!("Could not decode cover image for thumbnail: {error}"))?;

    let rgba = dynamic_img.to_rgba8();
    let mut rgb = RgbImage::new(rgba.width(), rgba.height());

    for (x, y, pixel) in rgba.enumerate_pixels() {
        let alpha = pixel[3] as f32 / 255.0;
        let r = ((pixel[0] as f32 * alpha) + 255.0 * (1.0 - alpha)) as u8;
        let g = ((pixel[1] as f32 * alpha) + 255.0 * (1.0 - alpha)) as u8;
        let b = ((pixel[2] as f32 * alpha) + 255.0 * (1.0 - alpha)) as u8;
        rgb.put_pixel(x, y, Rgb([r, g, b]));
    }

    let mut thumb_jpeg = Vec::new();
    DynamicImage::ImageRgb8(rgb)
        .write_to(&mut Cursor::new(&mut thumb_jpeg), ImageFormat::Jpeg)
        .map_err(|error| anyhow!("Failed to encode JPEG thumbnail: {error}"))?;

    Ok(thumb_jpeg)
}

/// Sanitizes a string to be a valid WhatsApp sticker pack ID.
pub fn sanitize_pack_id(name: &str) -> String {
    let clean: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let clean = clean.trim_matches('_');
    if clean.is_empty() {
        "sticker_pack".to_string()
    } else if clean.len() > 60 {
        clean[..60].to_string()
    } else {
        clean.to_string()
    }
}

/// Fetches, splits, and builds WhatsApp sticker packs from a Telegram sticker set.
pub async fn convert_telegram_pack(
    client: &Client,
    pack_slug: &str,
    custom_name: Option<String>,
) -> Result<Vec<wa::Message>> {
    let token = telegram_token()?;
    let base = telegram_api_base();
    let http = http_client()?;

    let set = fetch_sticker_set(http, &base, &token, pack_slug).await?;

    if set.stickers.is_empty() {
        bail!("Telegram sticker set `{}` contains no stickers.", set.name);
    }

    // Auto-split stickers into chunks of at most MAX_STICKERS_PER_PACK
    let chunks: Vec<&[TgSticker]> = set.stickers.chunks(MAX_STICKERS_PER_PACK).collect();
    let total_packs = chunks.len();

    let base_name = custom_name.unwrap_or(set.title);

    let mut result_messages = Vec::new();
    let semaphore = Arc::new(Semaphore::new(6));

    for (index, chunk) in chunks.into_iter().enumerate() {
        let stickers = convert_chunk(http, &base, &token, chunk, semaphore.clone()).await?;

        let pack_name = if total_packs == 1 || index == 0 {
            base_name.clone()
        } else {
            format!("{base_name} {}", index + 1)
        };
        let pack_id = format!(
            "{}_{}_{}",
            sanitize_pack_id(&set.name),
            index + 1,
            uuid::Uuid::new_v4().simple()
        );
        let msg = build_pack_message(client, &pack_id, &pack_name, stickers).await?;
        result_messages.push(msg);
    }

    Ok(result_messages)
}

/// Downloads and converts one pack chunk's stickers concurrently, bounded by
/// `semaphore`, and answers with them in the order Telegram listed them.
async fn convert_chunk(
    http: &'static reqwest::Client,
    base: &str,
    token: &str,
    stickers: &[TgSticker],
    semaphore: Arc<Semaphore>,
) -> Result<Vec<(Vec<u8>, Option<String>)>> {
    let count = stickers.len();
    let mut tasks = JoinSet::new();
    for (index, sticker) in stickers.iter().enumerate() {
        let base = base.to_string();
        let token = token.to_string();
        let sticker = sticker.clone();
        let semaphore = semaphore.clone();
        tasks.spawn(async move {
            let _permit = semaphore
                .acquire_owned()
                .await
                .map_err(|error| anyhow!("{error}"))?;
            let path = get_file_path(http, &base, &token, &sticker.file_id).await?;
            let data = download_file(http, &base, &token, &path).await?;
            let converted = prepare_sticker_data(&data, sticker.is_video).await?;
            Ok::<_, Error>((index, (converted, sticker.emoji)))
        });
    }

    // Results arrive as each sticker finishes, so they are placed back at their
    // own index; a failure drops the set, which aborts the stickers still in
    // flight.
    let mut ordered: Vec<Option<(Vec<u8>, Option<String>)>> = (0..count).map(|_| None).collect();
    while let Some(result) = tasks.join_next().await {
        let (index, value) = match result {
            Ok(Ok(value)) => value,
            Ok(Err(error)) => return Err(error),
            Err(error) => return Err(anyhow!("Sticker download task failed: {error}")),
        };
        let Some(slot) = ordered.get_mut(index) else {
            bail!("Sticker conversion task returned an invalid index.");
        };
        *slot = Some(value);
    }

    ordered
        .into_iter()
        .map(|sticker| sticker.ok_or_else(|| anyhow!("Sticker conversion task returned no result")))
        .collect()
}

/// Builds and uploads one WhatsApp sticker pack from its converted stickers.
async fn build_pack_message(
    client: &Client,
    pack_id: &str,
    pack_name: &str,
    chunk_stickers: Vec<(Vec<u8>, Option<String>)>,
) -> Result<wa::Message> {
    if chunk_stickers.is_empty() {
        bail!("Sticker pack chunk is empty.");
    }

    let cover_webp = &chunk_stickers[0].0;
    let thumb_jpeg = create_jpeg_thumbnail(cover_webp)?;
    let sticker_inputs: Vec<StickerInput> = chunk_stickers
        .iter()
        .map(|(data, emoji)| {
            let mut input = StickerInput::new(data);
            if let Some(emoji) = emoji {
                input = input.with_emojis(vec![emoji.clone()]);
            }
            input
        })
        .collect();

    let zip_result = create_sticker_pack_zip(pack_id, &sticker_inputs, cover_webp)
        .map_err(|error| anyhow!("Failed to create sticker pack ZIP: {error}"))?;
    let media_key: [u8; 32] = rand::random();
    let (zip_upload, thumb_upload) = tokio::try_join!(
        client.upload(
            zip_result.zip_bytes.clone(),
            MediaType::StickerPack,
            UploadOptions::new().with_media_key(media_key),
        ),
        client.upload(
            thumb_jpeg,
            MediaType::StickerPackThumbnail,
            UploadOptions::new().with_media_key(media_key),
        ),
    )
    .map_err(|error| anyhow!("Failed to upload sticker pack media: {error}"))?;

    let metadata =
        StickerPackMetadata::new(pack_id.to_string(), pack_name.to_string(), "Megumi".into());
    build_sticker_pack_message(
        &zip_result,
        &zip_upload.into(),
        &thumb_upload.into(),
        metadata,
    )
    .map_err(|error| anyhow!("Failed to build sticker pack message: {error}"))
}
