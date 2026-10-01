use megumi_whatsapp::commands::sticker::source::{Decision, decide};

#[test]
fn a_url_argument_is_the_source_even_when_media_is_attached() {
    assert_eq!(
        decide(Some("https://example.com/cat.png"), true).unwrap(),
        Decision::Url("https://example.com/cat.png".into())
    );
}

#[test]
fn leftover_words_do_not_shadow_attached_media() {
    assert_eq!(decide(Some("please"), true).unwrap(), Decision::Attachment);
}

#[test]
fn leftover_words_without_media_are_rejected() {
    let error = decide(Some("please"), false).unwrap_err();
    assert_eq!(error, "`please` is not an http or https address.");
}

#[test]
fn no_argument_and_no_media_is_usage() {
    let error = decide(None, false).unwrap_err();
    assert!(error.contains("!sticker"), "{error}");
}

#[test]
fn no_argument_with_media_uses_the_attachment() {
    assert_eq!(decide(None, true).unwrap(), Decision::Attachment);
}

#[test]
fn only_http_addresses_count_as_a_url_argument() {
    assert_eq!(
        decide(Some("https://cdn.example/sticker.webp"), false).unwrap(),
        Decision::Url("https://cdn.example/sticker.webp".into())
    );
    assert_eq!(
        decide(Some("http://cdn.example/sticker.webp"), false).unwrap(),
        Decision::Url("http://cdn.example/sticker.webp".into())
    );
    assert_eq!(
        decide(Some("  https://cdn.example/sticker.webp  "), false).unwrap(),
        Decision::Url("https://cdn.example/sticker.webp".into())
    );
}
