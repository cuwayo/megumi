//! Turning a voice note or an image into text, with an OpenAI-compatible API.
//!
//! WhatsApp delivers media as encrypted bytes; the model cannot read a pointer.
//! This is the piece that closes the gap: it sends audio to a transcription
//! endpoint and an image to a vision endpoint, and hands back a short text
//! description the adapter stores on the message. It lives in the bot crate
//! because its only caller is the adapter, which is the one place that holds
//! both the bytes and the client to fetch them.
//!
//! The provider is optional. [`OpenAiMedia::from_env`] returns `None` without a
//! credential, and the adapter then describes nothing — the bot behaves exactly
//! as it did before this module existed. Both endpoints are the OpenAI shape, so
//! any compatible host (OpenAI, a gateway, a local server) works by pointing
//! `MEDIA_API_BASE` at it.

use base64::{Engine as _, engine::general_purpose::STANDARD};

/// The default vision prompt: a plain description, not a conversation.
const VISION_PROMPT: &str = "Describe this image in one or two sentences for a chat assistant's memory. \
     Say what is shown, including any text, and nothing else.";

/// The default API host, which already ends in `/v1`.
const DEFAULT_BASE: &str = "https://api.openai.com/v1";

/// An OpenAI-compatible transcription and vision provider.
pub struct OpenAiMedia {
    api_key: String,
    api_base: String,
    http: reqwest::Client,
    transcribe_model: String,
    vision_model: String,
    max_chars: usize,
    max_tokens: u32,
    max_bytes: u64,
}

impl OpenAiMedia {
    /// Builds the provider from the environment, or `None` without a key.
    ///
    /// Reads `MEDIA_API_KEY` first, then `OPENAI_API_KEY` as a fallback. A
    /// missing or empty key returns `None`, so an unconfigured bot simply has no
    /// media understanding rather than failing to start. Every other setting has
    /// a default, so only the key is required.
    pub fn from_env() -> Option<Self> {
        let api_key = ["MEDIA_API_KEY", "OPENAI_API_KEY"]
            .iter()
            .find_map(|key| std::env::var(key).ok())
            .filter(|key| !key.trim().is_empty())?;
        let api_base = std::env::var("MEDIA_API_BASE")
            .ok()
            .filter(|base| !base.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_BASE.to_string());
        Some(Self {
            api_key,
            api_base,
            http: reqwest::Client::builder()
                // A long voice note can take a while to transcribe.
                .timeout(std::time::Duration::from_secs(120))
                .user_agent("megumi")
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            transcribe_model: env_or("MEDIA_TRANSCRIBE_MODEL", "whisper-1"),
            vision_model: env_or("MEDIA_VISION_MODEL", "gpt-4o-mini"),
            max_chars: env_usize("MEDIA_MAX_CHARS").unwrap_or(1_000),
            max_tokens: env_usize("MEDIA_MAX_TOKENS")
                .map(|tokens| tokens as u32)
                .unwrap_or(300),
            max_bytes: env_usize("MEDIA_MAX_BYTES")
                .map(|bytes| bytes as u64)
                .unwrap_or(20_000_000),
        })
    }

    /// The largest media file worth sending, in bytes.
    ///
    /// The adapter checks this before downloading, so an oversized file is
    /// refused without paying for the download.
    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    /// Transcribes an audio clip to text.
    ///
    /// POSTs a multipart body to `{base}/audio/transcriptions` — the shape the
    /// OpenAI whisper endpoint expects — and reads the `text` field back.
    pub async fn transcribe(
        &self,
        bytes: Vec<u8>,
        mimetype: Option<&str>,
    ) -> Result<String, String> {
        let file_name = file_name_for(mimetype);
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(file_name)
            .mime_str(mimetype.unwrap_or("application/octet-stream"))
            .map_err(|error| format!("the audio could not be prepared: {error}"))?;
        let form = reqwest::multipart::Form::new()
            .part("file", part)
            .text("model", self.transcribe_model.clone());

        let response = self
            .http
            .post(transcription_url(&self.api_base))
            .bearer_auth(&self.api_key)
            .multipart(form)
            .send()
            .await
            .map_err(|error| format!("the transcription request failed: {error}"))?;
        let body = read_body(response).await?;
        let parsed: Transcription = serde_json::from_str(&body)
            .map_err(|error| format!("the transcription response could not be read: {error}"))?;
        Ok(self.cap(parsed.text))
    }

    /// Describes an image in text.
    ///
    /// POSTs a one-message chat completion carrying the image as a base64 data
    /// URI — the OpenAI vision shape — and reads the reply back.
    pub async fn describe_image(
        &self,
        bytes: Vec<u8>,
        mimetype: Option<&str>,
    ) -> Result<String, String> {
        let data_uri = format!(
            "data:{};base64,{}",
            mimetype.unwrap_or("image/jpeg"),
            STANDARD.encode(&bytes)
        );
        let body = serde_json::json!({
            "model": self.vision_model,
            "max_tokens": self.max_tokens,
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "text", "text": VISION_PROMPT },
                    { "type": "image_url", "image_url": { "url": data_uri } }
                ]
            }]
        });

        let response = self
            .http
            .post(chat_url(&self.api_base))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|error| format!("the image description request failed: {error}"))?;
        let body = read_body(response).await?;
        let parsed: ChatCompletion = serde_json::from_str(&body)
            .map_err(|error| format!("the image description could not be read: {error}"))?;
        let text = parsed
            .choices
            .into_iter()
            .next()
            .and_then(|choice| choice.message)
            .and_then(|message| message.content)
            .unwrap_or_default();
        Ok(self.cap(text))
    }

    /// Trims a provider reply to the configured cap, never splitting a char.
    fn cap(&self, text: String) -> String {
        let trimmed = text.trim();
        if trimmed.chars().count() <= self.max_chars {
            return trimmed.to_string();
        }
        trimmed.chars().take(self.max_chars).collect()
    }
}

/// Reads a response body, turning a non-success status into an error.
///
/// The provider's own message is kept in the error so a failure is diagnosable
/// from the log, but it is never sent to a chat.
async fn read_body(response: reqwest::Response) -> Result<String, String> {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "the media provider returned HTTP {}: {}",
            status.as_u16(),
            body.chars().take(200).collect::<String>()
        ));
    }
    Ok(body)
}

/// The OpenAI transcription response, reduced to its text.
#[derive(serde::Deserialize)]
struct Transcription {
    #[serde(default)]
    text: String,
}

/// The OpenAI chat-completion response, reduced to the reply text.
#[derive(serde::Deserialize)]
struct ChatCompletion {
    #[serde(default)]
    choices: Vec<Choice>,
}

#[derive(serde::Deserialize)]
struct Choice {
    #[serde(default)]
    message: Option<Message>,
}

#[derive(serde::Deserialize)]
struct Message {
    #[serde(default)]
    content: Option<String>,
}

/// The transcription endpoint for `api_base`, which may or may not end in `/v1`.
fn transcription_url(api_base: &str) -> String {
    format!("{}/audio/transcriptions", api_base.trim_end_matches('/'))
}

/// The chat-completions endpoint for `api_base`.
fn chat_url(api_base: &str) -> String {
    format!("{}/chat/completions", api_base.trim_end_matches('/'))
}

/// A filename for a multipart audio part, chosen so the API can guess the format.
///
/// The API reads the extension, not the declared MIME type, so the common audio
/// types map to an extension it recognises; anything else is offered as `.ogg`,
/// which the endpoints accept.
fn file_name_for(mimetype: Option<&str>) -> String {
    let extension = match mimetype {
        Some(mime) if mime.contains("wav") => "wav",
        Some(mime) if mime.contains("mpeg") || mime.contains("mp3") => "mp3",
        Some(mime) if mime.contains("mp4") || mime.contains("m4a") => "m4a",
        Some(mime) if mime.contains("webm") => "webm",
        Some(mime) if mime.contains("flac") => "flac",
        _ => "ogg",
    };
    format!("audio.{extension}")
}

/// Reads a non-empty environment variable, or `default`.
fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn env_usize(key: &str) -> Option<usize> {
    std::env::var(key).ok()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_endpoints_are_built_without_doubling_v1() {
        assert_eq!(
            transcription_url("https://api.openai.com/v1"),
            "https://api.openai.com/v1/audio/transcriptions"
        );
        assert_eq!(
            transcription_url("https://api.openai.com/v1/"),
            "https://api.openai.com/v1/audio/transcriptions"
        );
        assert_eq!(
            chat_url("https://proxy.example/v1"),
            "https://proxy.example/v1/chat/completions"
        );
    }

    #[test]
    fn an_audio_file_name_follows_the_mimetype() {
        assert_eq!(file_name_for(Some("audio/ogg; codecs=opus")), "audio.ogg");
        assert_eq!(file_name_for(Some("audio/mpeg")), "audio.mp3");
        assert_eq!(file_name_for(Some("audio/wav")), "audio.wav");
        assert_eq!(file_name_for(None), "audio.ogg");
    }

    #[test]
    fn a_transcription_response_is_read() {
        let parsed: Transcription = serde_json::from_str(r#"{"text":"hello there"}"#).unwrap();
        assert_eq!(parsed.text, "hello there");
        // A response with no text is empty, not an error.
        let parsed: Transcription = serde_json::from_str("{}").unwrap();
        assert!(parsed.text.is_empty());
    }

    #[test]
    fn a_chat_completion_response_is_read() {
        let parsed: ChatCompletion = serde_json::from_str(
            r#"{"choices":[{"message":{"role":"assistant","content":"a red bicycle"}}]}"#,
        )
        .unwrap();
        let text = parsed
            .choices
            .into_iter()
            .next()
            .and_then(|choice| choice.message)
            .and_then(|message| message.content)
            .unwrap_or_default();
        assert_eq!(text, "a red bicycle");
    }

    #[test]
    fn the_description_is_capped_without_splitting_a_char() {
        let media = OpenAiMedia {
            api_key: "key".into(),
            api_base: DEFAULT_BASE.into(),
            http: reqwest::Client::new(),
            transcribe_model: "whisper-1".into(),
            vision_model: "gpt-4o-mini".into(),
            max_chars: 5,
            max_tokens: 300,
            max_bytes: 20_000_000,
        };
        assert_eq!(media.cap("  abcdefgh  ".into()), "abcde");
        assert_eq!(media.cap("abc".into()), "abc");
    }

    #[test]
    fn from_env_is_none_without_a_key() {
        // The test binary owns these variables; clear them so the assertion is
        // about the absent-key path and not the ambient environment.
        unsafe {
            std::env::remove_var("MEDIA_API_KEY");
            std::env::remove_var("OPENAI_API_KEY");
        }
        assert!(OpenAiMedia::from_env().is_none());
    }
}
