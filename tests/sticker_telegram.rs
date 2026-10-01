use std::io::Cursor;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use image::{DynamicImage, ImageFormat};
use megumi_whatsapp::commands::sticker::telegram::{
    ATTEMPTS, create_jpeg_thumbnail, extract_pack_name, fetch_sticker_set, sanitize_pack_id,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[test]
fn extracts_pack_name_from_various_url_formats() {
    assert_eq!(
        extract_pack_name("https://t.me/addstickers/NachoTheCat"),
        Some("NachoTheCat")
    );
    assert_eq!(
        extract_pack_name("http://t.me/addstickers/NachoTheCat?start=123"),
        Some("NachoTheCat")
    );
    assert_eq!(
        extract_pack_name("t.me/addstickers/NachoTheCat"),
        Some("NachoTheCat")
    );
    assert_eq!(
        extract_pack_name("https://telegram.me/addstickers/animals"),
        Some("animals")
    );
    assert_eq!(
        extract_pack_name("tg://addstickers?set=CuteCats"),
        Some("CuteCats")
    );
    assert_eq!(extract_pack_name("https://example.com/sticker.png"), None);
}

#[test]
fn sanitizes_pack_id() {
    assert_eq!(sanitize_pack_id("my/invalid.pack!id"), "my_invalid_pack_id");
    assert_eq!(sanitize_pack_id("valid-pack_123"), "valid-pack_123");
    assert_eq!(sanitize_pack_id(""), "sticker_pack");
}

#[test]
fn creates_jpeg_thumbnail_from_rgba() {
    // Create a 10x10 transparent image in memory
    let img = DynamicImage::ImageRgba8(image::RgbaImage::new(10, 10));
    let mut webp = Vec::new();
    img.write_to(&mut Cursor::new(&mut webp), ImageFormat::Png)
        .unwrap();

    let jpeg = create_jpeg_thumbnail(&webp).unwrap();
    assert!(!jpeg.is_empty());
    assert_eq!(&jpeg[0..2], &[0xFF, 0xD8]); // JPEG magic header
}

/// Serves `script` one response per request on a local port, and answers
/// with the base url to send them to plus how many requests arrived.
async fn mock_telegram(script: Vec<String>) -> (String, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let served = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&served);

    tokio::spawn(async move {
        for response in script {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let mut request = [0u8; 4096];
            let _ = socket.read(&mut request).await;
            counter.fetch_add(1, Ordering::SeqCst);
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
    });

    (base, served)
}

fn http_response(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
}

#[tokio::test]
async fn a_throttled_sticker_set_fetch_is_repeated_until_it_answers() {
    let set = r#"{"ok":true,"result":{"name":"cats","title":"Cats","stickers":
        [{"file_id":"f","file_unique_id":"u","width":512,"height":512}]}}"#;
    let (base, requests) = mock_telegram(vec![
        http_response("500 Internal Server Error", r#"{"ok":false}"#),
        http_response("429 Too Many Requests", r#"{"ok":false}"#),
        http_response("200 OK", set),
    ])
    .await;

    let set = fetch_sticker_set(&reqwest::Client::new(), &base, "SECRET", "cats")
        .await
        .unwrap();

    assert_eq!(set.name, "cats");
    assert_eq!(set.stickers.len(), 1);
    assert_eq!(requests.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn a_refused_sticker_set_fetch_is_not_repeated() {
    let (base, requests) = mock_telegram(vec![http_response(
        "400 Bad Request",
        r#"{"ok":false,"error_code":400,"description":"Bad Request: STICKERSET_INVALID"}"#,
    )])
    .await;

    let error = fetch_sticker_set(&reqwest::Client::new(), &base, "SECRET", "nope")
        .await
        .unwrap_err()
        .to_string();

    assert!(error.contains("STICKERSET_INVALID"), "{error}");
    assert_eq!(requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_failed_fetch_never_hands_the_bot_token_to_the_chat() {
    // A port nothing listens on, so the request fails in transport: that is
    // the failure that used to report the url the token travels in.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);

    let error = fetch_sticker_set(&reqwest::Client::new(), &base, "SECRET-TOKEN", "cats")
        .await
        .unwrap_err()
        .to_string();

    assert!(!error.contains("SECRET-TOKEN"), "{error}");
    assert!(!error.contains(&base), "{error}");
}

#[tokio::test]
async fn a_non_json_failure_reports_the_status_it_came_with() {
    let (base, _) = mock_telegram(vec![
        http_response("502 Bad Gateway", "bad gateway");
        ATTEMPTS as usize
    ])
    .await;

    let error = fetch_sticker_set(&reqwest::Client::new(), &base, "SECRET", "cats")
        .await
        .unwrap_err()
        .to_string();

    assert!(error.contains("502"), "{error}");
}
