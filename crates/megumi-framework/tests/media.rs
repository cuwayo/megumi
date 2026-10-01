use megumi::{Attachment, MediaKind, MediaSpec, Uploaded, image_mimetype};
use whatsapp_rust::prelude::{MessageBuilderExt, MessageField, wa};

fn upload() -> Uploaded {
    Uploaded {
        url: "https://media.example/url".to_string(),
        direct_path: "/v/t62.7118-24/abc".to_string(),
        media_key: [7; 32],
        file_enc_sha256: [8; 32],
        file_sha256: [9; 32],
        file_length: 1234,
        media_key_timestamp: 1_700_000_000,
        streaming_sidecar: None,
    }
}

fn spec(kind: MediaKind) -> MediaSpec {
    MediaSpec {
        kind,
        mimetype: None,
        file_name: None,
        voice_note: false,
        seconds: None,
        size: None,
        thumbnail: None,
        animated: false,
    }
}

#[test]
fn tells_the_image_formats_apart() {
    assert_eq!(image_mimetype(b"\x89PNG\r\n\x1a\n...."), "image/png");
    assert_eq!(image_mimetype(&[0xFF, 0xD8, 0xFF, 0xE0]), "image/jpeg");
    assert_eq!(image_mimetype(b"GIF89a......"), "image/gif");

    let mut webp = Vec::from(*b"RIFF\x00\x00\x00\x00WEBPVP8 ");
    webp.extend_from_slice(&[0; 8]);
    assert_eq!(image_mimetype(&webp), "image/webp");

    assert_eq!(image_mimetype(b"not an image"), "image/jpeg");
}

#[test]
fn an_image_stanza_carries_its_caption_size_and_quote() {
    let mut spec = spec(MediaKind::Image);
    spec.size = Some((640, 480));
    spec.thumbnail = Some(vec![0xFF, 0xD8]);

    let message = spec
        .stanza(
            upload(),
            "image/png".to_string(),
            Some("a caption".to_string()),
            Some(wa::ContextInfo::default()),
        )
        .expect("an image carries a caption");

    let image = message.image_message.as_option().expect("an image stanza");
    assert_eq!(image.mimetype.as_deref(), Some("image/png"));
    assert_eq!(image.caption.as_deref(), Some("a caption"));
    assert_eq!((image.width, image.height), (Some(640), Some(480)));
    assert_eq!(
        image.jpeg_thumbnail.as_deref(),
        Some([0xFF, 0xD8].as_slice())
    );
    assert!(image.context_info.is_set());
    assert_eq!(image.file_length, Some(1234));
    assert_eq!(image.media_key.as_deref(), Some([7; 32].as_slice()));
    assert_eq!(image.direct_path.as_deref(), Some("/v/t62.7118-24/abc"));
}

#[test]
fn a_voice_note_is_marked_push_to_talk() {
    let mut spec = spec(MediaKind::Audio);
    spec.voice_note = true;
    spec.seconds = Some(3);

    let message = spec
        .stanza(upload(), "audio/ogg; codecs=opus".to_string(), None, None)
        .expect("a voice note carries no caption");

    let audio = message.audio_message.as_option().expect("an audio stanza");
    assert_eq!(audio.ptt, Some(true));
    assert_eq!(audio.seconds, Some(3));
    assert_eq!(audio.mimetype.as_deref(), Some("audio/ogg; codecs=opus"));
}

#[test]
fn a_sticker_stanza_carries_its_animation_flag() {
    let mut spec = spec(MediaKind::Sticker);
    spec.animated = true;
    spec.size = Some((512, 512));

    let message = spec
        .stanza(upload(), "image/webp".to_string(), None, None)
        .expect("a sticker carries no caption");

    let sticker = message
        .sticker_message
        .as_option()
        .expect("a sticker stanza");
    assert_eq!(sticker.is_animated, Some(true));
    assert_eq!((sticker.width, sticker.height), (Some(512), Some(512)));
}

#[test]
fn a_stanza_without_a_caption_field_refuses_one() {
    for kind in [MediaKind::Sticker, MediaKind::Audio] {
        let error = spec(kind)
            .stanza(
                upload(),
                "application/octet-stream".to_string(),
                Some("text".to_string()),
                None,
            )
            .expect_err("a caption has nowhere to go on this stanza");

        assert!(
            error.to_string().contains("cannot carry a caption"),
            "{error}"
        );
    }
}

fn image(caption: &str, file_length: u64) -> wa::Message {
    wa::Message {
        image_message: MessageField::some(wa::message::ImageMessage {
            caption: Some(caption.to_string()),
            mimetype: Some("image/png".to_string()),
            file_length: Some(file_length),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// A text reply that quotes `quoted`, the shape WhatsApp sends a reply in.
fn quoting(quoted: wa::Message) -> wa::Message {
    wa::Message {
        extended_text_message: MessageField::some(wa::message::ExtendedTextMessage {
            text: Some("!sticker".to_string()),
            context_info: MessageField::some(wa::ContextInfo {
                quoted_message: MessageField::some(quoted),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[test]
fn reads_media_from_the_message_or_the_one_it_quotes() {
    let own = image("here", 10);
    let attachment = Attachment::from_message(&own).expect("an image");
    assert_eq!(attachment.kind(), MediaKind::Image);
    assert_eq!(attachment.caption(), Some("here"));
    assert_eq!(attachment.mimetype(), Some("image/png"));
    assert_eq!(attachment.file_length(), Some(10));
    assert_eq!(attachment.file_name(), None);

    let reply = quoting(image("quoted", 20));
    let attachment = Attachment::from_message(&reply).expect("the quoted image");
    assert_eq!(attachment.caption(), Some("quoted"));

    assert!(Attachment::from_message(&wa::Message::text("!sticker")).is_none());

    let document = wa::Message {
        document_message: MessageField::some(wa::message::DocumentMessage {
            file_name: Some("report.pdf".to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let attachment = Attachment::from_message(&document).expect("a file");
    assert_eq!(attachment.kind(), MediaKind::Document);
    assert_eq!(attachment.file_name(), Some("report.pdf"));
}
