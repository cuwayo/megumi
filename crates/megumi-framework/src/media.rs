//! The media a command receives, and the media a reply attaches.
//!
//! Poise pairs the outgoing `CreateAttachment` a reply carries with the incoming
//! `serenity::Attachment` a command parameter holds. A WhatsApp message carries
//! at most one media payload, and a command reaches the one it works on either
//! on the message itself or on the message a reply quotes, so the framework
//! offers the same two halves: [`CreateAttachment`], attached to a
//! [`CreateReply`](crate::CreateReply), to send media, and [`Attachment`] to
//! read what arrived.

use whatsapp_rust::download::{Downloadable, MediaType};
use whatsapp_rust::prelude::MessageExt;
use whatsapp_rust::prelude::{MessageField, wa};
use whatsapp_rust::upload::{UploadOptions, UploadResponse};

use crate::context::{Context, FrameworkData};
use crate::error::Error;

/// Fills the CDN references every media stanza carries, taken from one upload.
///
/// A macro rather than a function because each message type is its own struct;
/// all of them name these fields the same way.
macro_rules! uploaded {
    ($message:ident, $upload:ident) => {
        $message.url = Some($upload.url);
        $message.direct_path = Some($upload.direct_path);
        $message.media_key = Some($upload.media_key.to_vec());
        $message.file_sha256 = Some($upload.file_sha256.to_vec());
        $message.file_enc_sha256 = Some($upload.file_enc_sha256.to_vec());
        $message.file_length = Some($upload.file_length);
        $message.media_key_timestamp = Some($upload.media_key_timestamp);
    };
}

/// The kind of media a message carries, and the kind a reply attaches.
///
/// WhatsApp has a stanza per kind, so a command states the kind instead of the
/// framework guessing it from the bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MediaKind {
    /// A still image.
    Image,
    /// A video clip.
    Video,
    /// An audio clip or voice note.
    Audio,
    /// A file attachment of any type.
    Document,
    /// A WhatsApp sticker.
    Sticker,
}

impl MediaKind {
    /// What the chat is told this media is when a step fails.
    pub fn label(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Document => "document",
            Self::Sticker => "sticker",
        }
    }

    /// The CDN keys WhatsApp encrypts an upload of this kind under.
    fn upload_type(self) -> MediaType {
        match self {
            Self::Image => MediaType::Image,
            Self::Video => MediaType::Video,
            Self::Audio => MediaType::Audio,
            Self::Document => MediaType::Document,
            Self::Sticker => MediaType::Sticker,
        }
    }

    /// Whether the stanza WhatsApp sends for this kind has a caption field.
    fn has_caption(self) -> bool {
        matches!(self, Self::Image | Self::Video | Self::Document)
    }
}

/// The media a received message carries.
///
/// Ask the context for one with [`Context::attachment`], which reads the media
/// of the message itself, or of the message it replies to when the command
/// arrived with one. Its bytes come from [`Context::download`].
#[derive(Clone, Copy)]
pub struct Attachment<'a> {
    media: Media<'a>,
}

#[derive(Clone, Copy)]
enum Media<'a> {
    Image(&'a wa::message::ImageMessage),
    Video(&'a wa::message::VideoMessage),
    Audio(&'a wa::message::AudioMessage),
    Document(&'a wa::message::DocumentMessage),
    Sticker(&'a wa::message::StickerMessage),
}

impl<'a> Attachment<'a> {
    /// The media `message` carries, or the media of the message it quotes.
    pub fn from_message(message: &'a wa::Message) -> Option<Self> {
        Self::on(message).or_else(|| {
            quoted_message(message).and_then(|quoted| Self::on(quoted.get_base_message()))
        })
    }

    /// The media `message` itself carries, if it carries any.
    fn on(message: &'a wa::Message) -> Option<Self> {
        let media = if let Some(image) = message.image_message.as_option() {
            Media::Image(image)
        } else if let Some(video) = message.video_message.as_option() {
            Media::Video(video)
        } else if let Some(audio) = message.audio_message.as_option() {
            Media::Audio(audio)
        } else if let Some(document) = message.document_message.as_option() {
            Media::Document(document)
        } else {
            Media::Sticker(message.sticker_message.as_option()?)
        };

        Some(Self { media })
    }

    /// What kind of media this is.
    pub fn kind(&self) -> MediaKind {
        match &self.media {
            Media::Image(_) => MediaKind::Image,
            Media::Video(_) => MediaKind::Video,
            Media::Audio(_) => MediaKind::Audio,
            Media::Document(_) => MediaKind::Document,
            Media::Sticker(_) => MediaKind::Sticker,
        }
    }

    /// The name the sender gave a document.
    pub fn file_name(&self) -> Option<&'a str> {
        match &self.media {
            Media::Document(document) => document.file_name.as_deref(),
            _ => None,
        }
    }

    /// The MIME type the sender declared.
    pub fn mimetype(&self) -> Option<&'a str> {
        match &self.media {
            Media::Image(image) => image.mimetype.as_deref(),
            Media::Video(video) => video.mimetype.as_deref(),
            Media::Audio(audio) => audio.mimetype.as_deref(),
            Media::Document(document) => document.mimetype.as_deref(),
            Media::Sticker(sticker) => sticker.mimetype.as_deref(),
        }
    }

    /// The caption the sender put on the media.
    pub fn caption(&self) -> Option<&'a str> {
        match &self.media {
            Media::Image(image) => image.caption.as_deref(),
            Media::Video(video) => video.caption.as_deref(),
            Media::Document(document) => document.caption.as_deref(),
            Media::Audio(_) | Media::Sticker(_) => None,
        }
    }

    /// The size the sender declared, for refusing a download before it starts.
    pub fn file_length(&self) -> Option<u64> {
        match &self.media {
            Media::Image(image) => image.file_length,
            Media::Video(video) => video.file_length,
            Media::Audio(audio) => audio.file_length,
            Media::Document(document) => document.file_length,
            Media::Sticker(sticker) => sticker.file_length,
        }
    }

    /// The CDN references [`Context::download`] fetches this media with.
    pub(crate) fn downloadable(&self) -> &dyn Downloadable {
        match &self.media {
            Media::Image(image) => *image,
            Media::Video(video) => *video,
            Media::Audio(audio) => *audio,
            Media::Document(document) => *document,
            Media::Sticker(sticker) => *sticker,
        }
    }
}

/// The message a reply quotes. A reply is sent as text carrying the quote, and
/// a command reaching a quoted media message is that same shape.
fn quoted_message(message: &wa::Message) -> Option<&wa::Message> {
    message
        .extended_text_message
        .as_option()
        .and_then(|extended| extended.context_info.as_option())
        .and_then(|context| context.quoted_message.as_option())
}

/// Media a reply attaches.
///
/// WhatsApp sends one media payload per message, so a reply carries at most one
/// attachment: [`CreateReply::attachment`](crate::CreateReply::attachment)
/// replaces any set before it. The bytes are uploaded when the reply is sent.
#[derive(Clone)]
pub struct CreateAttachment {
    bytes: Vec<u8>,
    spec: MediaSpec,
}

/// Everything about a reply's media except the bytes, which the upload consumes.
#[derive(Clone)]
pub struct MediaSpec {
    /// The stanza WhatsApp sends for this media.
    pub kind: MediaKind,
    /// The MIME type declared to the receiver.
    pub mimetype: Option<String>,
    /// The name a document is offered under.
    pub file_name: Option<String>,
    /// Whether an audio attachment is a push-to-talk voice note.
    pub voice_note: bool,
    /// How long a video or audio clip runs.
    pub seconds: Option<u32>,
    /// The pixel dimensions of an image or video.
    pub size: Option<(u32, u32)>,
    /// The JPEG preview sent ahead of the full media.
    pub thumbnail: Option<Vec<u8>>,
    /// Whether a sticker is animated.
    pub animated: bool,
}

/// The CDN references one upload produced.
///
/// The framework keeps its own copy of the fields every media stanza wants
/// rather than holding `UploadResponse` itself, which callers cannot build.
#[derive(Clone)]
pub struct Uploaded {
    /// The CDN URL the encrypted blob was written to.
    pub url: String,
    /// The path within the CDN the blob was written to.
    pub direct_path: String,
    /// The key the media is encrypted with.
    pub media_key: [u8; 32],
    /// The SHA-256 of the plaintext.
    pub file_sha256: [u8; 32],
    /// The SHA-256 of the encrypted blob.
    pub file_enc_sha256: [u8; 32],
    /// The size of the plaintext in bytes.
    pub file_length: u64,
    /// When the media key was created.
    pub media_key_timestamp: i64,
    /// The sidecar an upload of streamable media carries.
    pub streaming_sidecar: Option<Vec<u8>>,
}

impl From<UploadResponse> for Uploaded {
    fn from(upload: UploadResponse) -> Self {
        Self {
            url: upload.url,
            direct_path: upload.direct_path,
            media_key: upload.media_key,
            file_sha256: upload.file_sha256,
            file_enc_sha256: upload.file_enc_sha256,
            file_length: upload.file_length,
            media_key_timestamp: upload.media_key_timestamp,
            streaming_sidecar: upload.streaming_sidecar,
        }
    }
}

impl CreateAttachment {
    /// An image, sent as an `imageMessage`.
    pub fn image(bytes: impl Into<Vec<u8>>) -> Self {
        Self::new(bytes, MediaKind::Image)
    }

    /// A video, sent as a `videoMessage`.
    pub fn video(bytes: impl Into<Vec<u8>>) -> Self {
        Self::new(bytes, MediaKind::Video)
    }

    /// An audio file, sent as an `audioMessage`. [`voice_note`](Self::voice_note)
    /// sends it as a push-to-talk recording instead.
    pub fn audio(bytes: impl Into<Vec<u8>>) -> Self {
        Self::new(bytes, MediaKind::Audio)
    }

    /// A file, sent as a `documentMessage` under `file_name`.
    pub fn document(bytes: impl Into<Vec<u8>>, file_name: impl Into<String>) -> Self {
        Self::new(bytes, MediaKind::Document).file_name(file_name)
    }

    /// A sticker, sent as a `stickerMessage`. WhatsApp renders a WebP animation
    /// only when `animated` matches the bytes, so the caller states it.
    pub fn sticker(bytes: impl Into<Vec<u8>>, animated: bool) -> Self {
        let mut attachment = Self::new(bytes, MediaKind::Sticker);
        attachment.spec.animated = animated;
        attachment
    }

    fn new(bytes: impl Into<Vec<u8>>, kind: MediaKind) -> Self {
        Self {
            bytes: bytes.into(),
            spec: MediaSpec {
                kind,
                mimetype: None,
                file_name: None,
                voice_note: false,
                seconds: None,
                size: None,
                thumbnail: None,
                animated: false,
            },
        }
    }

    /// The name WhatsApp shows for a document.
    #[must_use]
    pub fn file_name(mut self, file_name: impl Into<String>) -> Self {
        self.spec.file_name = Some(file_name.into());
        self
    }

    /// Overrides the MIME type WhatsApp is told. It defaults to the image's
    /// actual format, and to the kind's usual type otherwise.
    #[must_use]
    pub fn mimetype(mut self, mimetype: impl Into<String>) -> Self {
        self.spec.mimetype = Some(mimetype.into());
        self
    }

    /// Sends audio as a voice note: the push-to-talk flag `ptt`, under the
    /// `audio/ogg; codecs=opus` type a voice note has to keep.
    #[must_use]
    pub fn voice_note(mut self) -> Self {
        self.spec.voice_note = true;
        self
    }

    /// The duration WhatsApp shows for audio and video.
    #[must_use]
    pub fn seconds(mut self, seconds: u32) -> Self {
        self.spec.seconds = Some(seconds);
        self
    }

    /// The pixel size WhatsApp shows before the media itself loads.
    #[must_use]
    pub fn size(mut self, width: u32, height: u32) -> Self {
        self.spec.size = Some((width, height));
        self
    }

    /// A JPEG WhatsApp shows before the media itself loads.
    #[must_use]
    pub fn thumbnail(mut self, thumbnail: impl Into<Option<Vec<u8>>>) -> Self {
        self.spec.thumbnail = thumbnail.into();
        self
    }

    /// Uploads the bytes and returns the stanza WhatsApp should receive.
    pub(crate) async fn into_message<U: FrameworkData>(
        self,
        ctx: &Context<U>,
        caption: Option<String>,
        context: Option<wa::ContextInfo>,
    ) -> Result<wa::Message, Error> {
        let Self { bytes, spec } = self;
        let mimetype = spec
            .mimetype
            .clone()
            .unwrap_or_else(|| default_mimetype(spec.kind, spec.voice_note, &bytes));
        let label = spec.kind.label();

        let upload = ctx
            .message
            .client
            .upload(bytes, spec.kind.upload_type(), UploadOptions::new())
            .await
            .map_err(|error| format!("Could not upload the {label}: {error}"))?;

        spec.stanza(upload.into(), mimetype, caption, context)
    }
}

impl MediaSpec {
    /// The stanza WhatsApp receives for this media.
    pub fn stanza(
        &self,
        upload: Uploaded,
        mimetype: String,
        caption: Option<String>,
        context: Option<wa::ContextInfo>,
    ) -> Result<wa::Message, Error> {
        if caption.is_some() && !self.kind.has_caption() {
            return Err(format!("a {} cannot carry a caption", self.kind.label()).into());
        }

        let context_info = context.into();
        let (width, height) = self.size.map_or((None, None), |(w, h)| (Some(w), Some(h)));
        let thumbnail = self.thumbnail.clone();

        let message = match self.kind {
            MediaKind::Image => {
                let mut image = wa::message::ImageMessage {
                    mimetype: Some(mimetype),
                    caption,
                    width,
                    height,
                    jpeg_thumbnail: thumbnail,
                    context_info,
                    ..Default::default()
                };
                uploaded!(image, upload);
                wa::Message {
                    image_message: MessageField::some(image),
                    ..Default::default()
                }
            }
            MediaKind::Video => {
                let mut video = wa::message::VideoMessage {
                    mimetype: Some(mimetype),
                    caption,
                    seconds: self.seconds,
                    width,
                    height,
                    jpeg_thumbnail: thumbnail,
                    streaming_sidecar: upload.streaming_sidecar,
                    context_info,
                    ..Default::default()
                };
                uploaded!(video, upload);
                wa::Message {
                    video_message: MessageField::some(video),
                    ..Default::default()
                }
            }
            MediaKind::Audio => {
                let mut audio = wa::message::AudioMessage {
                    mimetype: Some(mimetype),
                    seconds: self.seconds,
                    ptt: Some(self.voice_note),
                    streaming_sidecar: upload.streaming_sidecar,
                    context_info,
                    ..Default::default()
                };
                uploaded!(audio, upload);
                wa::Message {
                    audio_message: MessageField::some(audio),
                    ..Default::default()
                }
            }
            MediaKind::Document => {
                let mut document = wa::message::DocumentMessage {
                    mimetype: Some(mimetype),
                    caption,
                    file_name: self.file_name.clone(),
                    jpeg_thumbnail: thumbnail,
                    context_info,
                    ..Default::default()
                };
                uploaded!(document, upload);
                wa::Message {
                    document_message: MessageField::some(document),
                    ..Default::default()
                }
            }
            MediaKind::Sticker => {
                let mut sticker = wa::message::StickerMessage {
                    mimetype: Some(mimetype),
                    width,
                    height,
                    is_animated: Some(self.animated),
                    context_info,
                    ..Default::default()
                };
                uploaded!(sticker, upload);
                wa::Message {
                    sticker_message: MessageField::some(sticker),
                    ..Default::default()
                }
            }
        };

        Ok(message)
    }
}

/// The MIME type WhatsApp is told this media is when the caller states none.
fn default_mimetype(kind: MediaKind, voice_note: bool, bytes: &[u8]) -> String {
    let mimetype = match kind {
        MediaKind::Image => image_mimetype(bytes),
        MediaKind::Video => "video/mp4",
        MediaKind::Audio if voice_note => "audio/ogg; codecs=opus",
        MediaKind::Audio => "audio/mp4",
        MediaKind::Document => "application/octet-stream",
        MediaKind::Sticker => "image/webp",
    };
    mimetype.to_string()
}

/// The image format WhatsApp is told an image is, read from its first bytes.
/// Media in a format this does not know is declared JPEG, the default a
/// caller overrides with [`CreateAttachment::mimetype`] when it knows better.
pub fn image_mimetype(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        "image/gif"
    } else {
        "image/jpeg"
    }
}
