//! The reply a command builds and sends.

use whatsapp_rust::download::MediaType;
use whatsapp_rust::prelude::MessageBuilderExt;
use whatsapp_rust::prelude::wa;
use whatsapp_rust::send::SendResult;
use whatsapp_rust::upload::UploadOptions;

use crate::context::{Context, FrameworkData};
use crate::error::Error;
use crate::media::CreateAttachment;

/// A card drawn above a text reply that is not a real link preview.
///
/// A [`LinkPreview`] only renders when its URL occurs in the message body,
/// because WhatsApp fetches the page itself. This card is the one a business
/// message attaches to an advertisement: the picture, the title, and the
/// address all travel inside the message, so nothing has to be written into the
/// text. The thumbnail is uploaded and its URL sent with the card, since the
/// raw bytes alone render as a blank square. Tapping it opens [`url`](Self::url).
///
/// [`large`](Self::large) draws the picture across the card instead of as the
/// small square beside the title.
#[derive(Clone, Debug)]
pub struct LinkCard {
    url: String,
    title: Option<String>,
    body: Option<String>,
    thumbnail: Option<Vec<u8>>,
    large: bool,
}

impl LinkCard {
    /// A card that opens `url`.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            title: None,
            body: None,
            thumbnail: None,
            large: false,
        }
    }

    /// The bold line of the card.
    #[must_use]
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// The smaller line under the title.
    #[must_use]
    pub fn body(mut self, body: impl Into<String>) -> Self {
        self.body = Some(body.into());
        self
    }

    /// The JPEG painted on the card.
    ///
    /// Carried inline, so it is the picture itself rather than a preview of an
    /// upload.
    #[must_use]
    pub fn thumbnail(mut self, thumbnail: impl Into<Vec<u8>>) -> Self {
        self.thumbnail = Some(thumbnail.into());
        self
    }

    /// Draws the thumbnail across the card.
    #[must_use]
    pub fn large(mut self, large: bool) -> Self {
        self.large = large;
        self
    }

    /// The `externalAdReply` context this card is sent as.
    ///
    /// The thumbnail is uploaded, because the raw bytes alone render as a blank
    /// square. The card has no fields for the path and the key, so `thumbnailUrl`
    /// carries the address the picture downloads from.
    fn context(
        self,
        thumbnail_url: Option<String>,
        context: Option<wa::ContextInfo>,
    ) -> wa::ContextInfo {
        let mut context = context.unwrap_or_default();
        context.external_ad_reply =
            whatsapp_rust::buffa::MessageField::some(wa::context_info::ExternalAdReplyInfo {
                title: self.title,
                body: self.body,
                media_type: Some(wa::context_info::external_ad_reply_info::MediaType::IMAGE),
                thumbnail_url,
                thumbnail: self.thumbnail,
                source_url: Some(self.url),
                render_larger_thumbnail: Some(self.large),
                ..Default::default()
            });
        context
    }
}

/// The link card shown above a text reply.
///
/// WhatsApp renders this as the small preview above the message body — a
/// thumbnail, a title, and a description — and it stays on the message even
/// when the chat has link previews turned off, because the card travels inside
/// the message rather than being fetched by the client. Tapping it opens
/// [`url`](LinkPreview::url).
///
/// The thumbnail has two halves. [`thumbnail`](Self::thumbnail) is a small JPEG
/// carried inline that shows immediately, so keep it to a couple of hundred
/// pixels on its longest edge. [`high_quality_thumbnail`](Self::high_quality_thumbnail)
/// is the full-size JPEG WhatsApp fetches once the card is on screen; it is
/// uploaded on its own, so it can be as large as the picture actually is.
#[derive(Clone, Debug)]
pub struct LinkPreview {
    url: String,
    title: Option<String>,
    description: Option<String>,
    thumbnail: Option<Vec<u8>>,
    high_quality_thumbnail: Option<Vec<u8>>,
    thumbnail_width: Option<u32>,
    thumbnail_height: Option<u32>,
}

impl LinkPreview {
    /// A preview that opens `url`.
    ///
    /// WhatsApp only shows the card when `url` occurs in the reply's text, and
    /// the text is sent exactly as given: the framework never adds the link.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            title: None,
            description: None,
            thumbnail: None,
            high_quality_thumbnail: None,
            thumbnail_width: None,
            thumbnail_height: None,
        }
    }

    /// The bold line of the card.
    #[must_use]
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// The smaller line under the title.
    #[must_use]
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// The small JPEG shown on the card straight away.
    #[must_use]
    pub fn thumbnail(mut self, thumbnail: impl Into<Vec<u8>>) -> Self {
        self.thumbnail = Some(thumbnail.into());
        self
    }

    /// The full-size JPEG WhatsApp loads in place of the small one.
    ///
    /// Unlike [`thumbnail`](Self::thumbnail) this is uploaded to WhatsApp's CDN
    /// when the reply is sent, so the card sharpens to the real picture instead
    /// of staying on the inline preview.
    #[must_use]
    pub fn high_quality_thumbnail(mut self, thumbnail: impl Into<Vec<u8>>) -> Self {
        self.high_quality_thumbnail = Some(thumbnail.into());
        self
    }

    /// The width, in pixels, sent as `thumbnailWidth` on the message.
    ///
    /// WhatsApp lays the card out from this rather than from the picture. Left
    /// unset, it is read out of the full-size thumbnail when one is uploaded.
    #[must_use]
    pub fn thumbnail_width(mut self, width: u32) -> Self {
        self.thumbnail_width = Some(width);
        self
    }

    /// The height, in pixels, sent as `thumbnailHeight` on the message.
    ///
    /// See [`thumbnail_width`](Self::thumbnail_width).
    #[must_use]
    pub fn thumbnail_height(mut self, height: u32) -> Self {
        self.thumbnail_height = Some(height);
        self
    }
}

/// A text message, promoted to an extended text message when it quotes another
/// message or carries a [`LinkPreview`].
///
/// The preview's URL is left entirely to the caller. `matchedText` only says
/// which of the body's links the card belongs to, so a preview whose URL never
/// appears in the text is dropped — the framework does not add it. A card that
/// must show without a link in the text is a [`LinkCard`].
#[cfg(test)]
mod tests {
    use super::jpeg_size;

    #[test]
    fn jpeg_size_reads_the_sof_marker() {
        // A minimal JPEG: SOI, an APP0 segment to skip, then the baseline SOF.
        let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00];
        bytes.extend_from_slice(&[
            0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x01, 0xF4, 0x04, 0x9B, 0x00, 0x00,
        ]);
        assert_eq!(jpeg_size(&bytes), Some((1179, 500)));
    }

    #[test]
    fn jpeg_size_rejects_bytes_that_are_not_a_jpeg() {
        assert_eq!(jpeg_size(&[0x89, 0x50, 0x4E, 0x47]), None);
        assert_eq!(jpeg_size(&[]), None);
    }

}

/// The CDN references of a full-size thumbnail, once it has been uploaded.
struct ThumbnailUpload {
    direct_path: String,
    sha256: Vec<u8>,
    enc_sha256: Vec<u8>,
    media_key: Vec<u8>,
    media_key_timestamp: i64,
    width: Option<u32>,
    height: Option<u32>,
}

impl ThumbnailUpload {
    /// The address the thumbnail downloads from.
    ///
    /// An upload's own URL is the endpoint the bytes were posted to, which is not
    /// fetchable. The picture itself is served from the media host at the path the
    /// upload returned.
    fn download_url(&self) -> String {
        format!("https://mmg.whatsapp.net{}", self.direct_path)
    }
}

/// The pixel size of a JPEG, read from its SOF marker.
///
/// WhatsApp picks the card's layout from this: a wide picture spans the card,
/// and without the size it falls back to the small thumbnail beside the title.
/// Reading the marker keeps the framework off an image decoder.
fn jpeg_size(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return None;
    }
    let mut index = 2;
    while index + 9 <= bytes.len() {
        if bytes[index] != 0xFF {
            return None;
        }
        let marker = bytes[index + 1];
        // Stuffed `FF` bytes and markers that carry no length.
        if marker == 0xFF || (0xD0..=0xD9).contains(&marker) {
            index += 2;
            continue;
        }
        let length = u16::from_be_bytes([bytes[index + 2], bytes[index + 3]]) as usize;
        // The start-of-frame markers, apart from the differential ones.
        if matches!(marker, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF)
            && index + 9 <= bytes.len()
        {
            let height = u16::from_be_bytes([bytes[index + 5], bytes[index + 6]]);
            let width = u16::from_be_bytes([bytes[index + 7], bytes[index + 8]]);
            return Some((u32::from(width), u32::from(height)));
        }
        index += 2 + length;
    }
    None
}

/// Uploads a link thumbnail.
///
/// A link thumbnail is its own media kind: it is encrypted under the link
/// thumbnail keys and stored on the `thumbnail-link` path, and a client
/// downloading it with the image keys gets nothing back. The card only needs
/// the path and the hashes, not the CDN URL.
async fn upload_thumbnail<U: FrameworkData>(
    bytes: Vec<u8>,
    ctx: &Context<U>,
) -> Result<ThumbnailUpload, Error> {
    let size = jpeg_size(&bytes);
    let upload = ctx
        .message
        .client
        .upload(bytes, MediaType::LinkThumbnail, UploadOptions::new())
        .await
        .map_err(|error| format!("Could not upload the link preview thumbnail: {error}"))?;
    Ok(ThumbnailUpload {
        direct_path: upload.direct_path,
        sha256: upload.file_sha256.to_vec(),
        enc_sha256: upload.file_enc_sha256.to_vec(),
        media_key: upload.media_key.to_vec(),
        media_key_timestamp: upload.media_key_timestamp,
        width: size.map(|(width, _)| width),
        height: size.map(|(_, height)| height),
    })
}

fn text_message(
    content: String,
    link: Option<LinkPreview>,
    thumbnail: Option<ThumbnailUpload>,
    context: Option<wa::ContextInfo>,
) -> wa::Message {
    let Some(link) = link else {
        return match context {
            Some(context) => wa::Message::text_with_context(content, context),
            None => wa::Message::text(content),
        };
    };

    wa::Message {
        extended_text_message: whatsapp_rust::buffa::MessageField::some(
            wa::message::ExtendedTextMessage {
                text: Some(content),
                matched_text: Some(link.url),
                title: link.title,
                description: link.description,
                preview_type: Some(wa::message::extended_text_message::PreviewType::NONE),
                jpeg_thumbnail: link.thumbnail,
                thumbnail_direct_path: thumbnail.as_ref().map(|upload| upload.direct_path.clone()),
                thumbnail_sha256: thumbnail.as_ref().map(|upload| upload.sha256.clone()),
                thumbnail_enc_sha256: thumbnail.as_ref().map(|upload| upload.enc_sha256.clone()),
                media_key: thumbnail.as_ref().map(|upload| upload.media_key.clone()),
                media_key_timestamp: thumbnail.as_ref().map(|upload| upload.media_key_timestamp),
                thumbnail_width: link
                    .thumbnail_width
                    .or_else(|| thumbnail.as_ref().and_then(|upload| upload.width)),
                thumbnail_height: link
                    .thumbnail_height
                    .or_else(|| thumbnail.as_ref().and_then(|upload| upload.height)),
                context_info: context.map_or_else(Default::default, |context| {
                    whatsapp_rust::buffa::MessageField::some(context)
                }),
                ..Default::default()
            },
        ),
        ..Default::default()
    }
}

/// What a command sends.
///
/// This is poise's `CreateReply`: a reply is described by chaining setters and
/// handed to [`Context::send`]. [`Context::say`] is the shorthand for the common
/// text-only reply.
///
/// ```no_run
/// use megumi::{Context, CreateAttachment, CreateReply, Error};
///
/// async fn send_a_picture(ctx: Context, bytes: Vec<u8>) -> Result<(), Error> {
///     ctx.send(
///         CreateReply::new()
///             .content("here it is")
///             .attachment(CreateAttachment::image(bytes))
///             .reply(true),
///     )
///     .await
/// }
/// ```
#[derive(Default, Clone)]
pub struct CreateReply {
    content: Option<String>,
    attachment: Option<CreateAttachment>,
    link: Option<LinkPreview>,
    card: Option<LinkCard>,
    reply: bool,
}

impl CreateReply {
    /// A reply with nothing in it yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// The text of the reply.
    ///
    /// When an attachment is present WhatsApp carries this as the media's
    /// caption rather than as a message of its own. A sticker or audio message
    /// has no caption field, so a reply attaching one is refused rather than
    /// dropping the text.
    #[must_use]
    pub fn content(mut self, content: impl Into<String>) -> Self {
        self.content = Some(content.into());
        self
    }

    /// The media the reply carries.
    ///
    /// WhatsApp sends one media payload per message, so this replaces any
    /// attachment set before it.
    #[must_use]
    pub fn attachment(mut self, attachment: CreateAttachment) -> Self {
        self.attachment = Some(attachment);
        self
    }

    /// Quotes the message the command replies to.
    #[must_use]
    pub fn reply(mut self, reply: bool) -> Self {
        self.reply = reply;
        self
    }

    /// Shows `preview` as the link card above the text.
    ///
    /// Only a text reply can carry one. A reply that also attaches media drops
    /// the preview, since WhatsApp sends a single payload and the attachment is
    /// what the recipient came for.
    #[must_use]
    pub fn link_preview(mut self, preview: LinkPreview) -> Self {
        self.link = Some(preview);
        self
    }

    /// Shows `card` above the text.
    ///
    /// Unlike [`link_preview`](Self::link_preview) this needs no link in the
    /// body, it survives a media attachment, and it replaces a link preview set
    /// before it.
    #[must_use]
    pub fn link_card(mut self, card: LinkCard) -> Self {
        self.card = Some(card);
        self
    }

    /// Uploads any attachment and sends the reply to the context's chat.
    pub(crate) async fn send<U: FrameworkData>(
        self,
        ctx: &Context<U>,
    ) -> Result<SendResult, Error> {
        let message = self.into_message(ctx, true).await?;
        ctx.send_raw_result(message).await
    }

    /// Replaces a message the command already sent, used by `reuse_response`.
    ///
    /// WhatsApp can only edit a text message into another text message, so a
    /// reply that attaches media is sent as a new message instead.
    pub(crate) async fn edit<U: FrameworkData>(
        self,
        ctx: &Context<U>,
        message_id: &str,
    ) -> Result<(), Error> {
        if self.attachment.is_some() {
            return self.send(ctx).await.map(drop);
        }
        let message = self.into_message(ctx, false).await?;
        ctx.message
            .client
            .edit_message(&ctx.message.info.source.chat, message_id, message)
            .await
            .map(drop)
            .map_err(Into::into)
    }

    async fn into_message<U: FrameworkData>(
        self,
        ctx: &Context<U>,
        quote: bool,
    ) -> Result<wa::Message, Error> {
        let Self {
            content,
            attachment,
            link,
            card,
            reply,
        } = self;
        let mut context = (quote && reply).then(|| ctx.message.build_quote_context());

        // The card rides the message context, so it survives a media attachment
        // where a link preview cannot. It also wins over a link preview: the two
        // are different ways to draw the same card.
        let has_card = card.is_some();
        if let Some(card) = card {
            let thumbnail_url = match card.thumbnail.clone() {
                Some(bytes) => Some(upload_thumbnail(bytes, ctx).await?.download_url()),
                None => None,
            };
            context = Some(card.context(thumbnail_url, context));
        }
        let link = (!has_card).then_some(link).flatten();

        // The full-size thumbnail is its own upload, done before the message is
        // built. An attached media payload wins, so the preview is not uploaded
        // at all when one is present.
        let thumbnail = match (&attachment, link.as_ref()) {
            (None, Some(link)) => match link.high_quality_thumbnail.clone() {
                Some(bytes) => Some(upload_thumbnail(bytes, ctx).await?),
                None => None,
            },
            _ => None,
        };

        match attachment {
            Some(attachment) => attachment.into_message(ctx, content, context).await,
            None => match content {
                Some(content) => Ok(text_message(content, link, thumbnail, context)),
                None => Err("a reply needs content or an attachment".into()),
            },
        }
    }
}
