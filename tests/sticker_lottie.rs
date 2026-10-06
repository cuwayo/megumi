use std::io::Write;

use flate2::{Compression, write::GzEncoder};
use megumi_whatsapp::commands::sticker::{lottie, transcode};
use webp_animation::Decoder;

fn tgs(json: &str) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(json.as_bytes()).unwrap();
    encoder.finish().unwrap()
}

/// A still 64x64 Lottie rectangle: the corners must stay transparent.
fn transparent_rectangle() -> Vec<u8> {
    tgs(r#"{
            "v":"5.7.4","fr":30,"ip":0,"op":2,"w":64,"h":64,
            "layers":[{
                "ty":4,"ip":0,"op":2,"st":0,"ks":{"o":{"a":0,"k":100},"r":{"a":0,"k":0},"p":{"a":0,"k":[0,0,0]},"a":{"a":0,"k":[0,0,0]},"s":{"a":0,"k":[100,100,100]}},
                "shapes":[
                    {"ty":"rc","p":{"a":0,"k":[32,32]},"s":{"a":0,"k":[32,32]},"r":{"a":0,"k":0}},
                    {"ty":"fl","c":{"a":0,"k":[1,0,0,1]},"o":{"a":0,"k":100}},
                    {"ty":"tr","p":{"a":0,"k":[0,0]},"a":{"a":0,"k":[0,0]},"s":{"a":0,"k":[100,100]},"r":{"a":0,"k":0},"o":{"a":0,"k":100}}
                ],"ao":0
            }]
        }"#)
}

/// A Lottie whose rectangle changes colour across frames while its box stays
/// opaque: the case that used to arrive with a black background.
fn pulsing_rectangle() -> Vec<u8> {
    tgs(r#"{
            "v":"5.7.4","fr":30,"ip":0,"op":10,"w":64,"h":64,
            "layers":[{
                "ty":4,"ip":0,"op":10,"st":0,
                "ks":{"o":{"a":0,"k":100},"r":{"a":0,"k":0},"p":{"a":0,"k":[0,0,0]},"a":{"a":0,"k":[0,0,0]},"s":{"a":0,"k":[100,100,100]}},
                "shapes":[
                    {"ty":"rc","p":{"a":0,"k":[32,32]},"s":{"a":0,"k":[32,32]},"r":{"a":0,"k":0}},
                    {"ty":"fl","c":{"a":1,"k":[{"t":0,"s":[1,0,0,1],"e":[0,0,1,1]},{"t":10,"s":[0,0,1,1]}]},"o":{"a":0,"k":100}},
                    {"ty":"tr","p":{"a":0,"k":[0,0]},"a":{"a":0,"k":[0,0]},"s":{"a":0,"k":[100,100]},"r":{"a":0,"k":0},"o":{"a":0,"k":100}}
                ],"ao":0
            }]
        }"#)
}

/// Decodes the sticker with libwebp and reports the alpha range over every frame.
fn decoded_alpha_range(sticker: &[u8]) -> (u8, u8) {
    let decoder = Decoder::new(sticker).unwrap();
    let mut min = 255u8;
    let mut max = 0u8;
    for frame in decoder {
        for pixel in frame.data().as_chunks::<4>().0 {
            min = min.min(pixel[3]);
            max = max.max(pixel[3]);
        }
    }
    (min, max)
}

#[tokio::test]
async fn lottie_stickers_keep_their_transparent_canvas() {
    let sticker = lottie::to_sticker(&transparent_rectangle()).await.unwrap();

    assert!(transcode::is_webp(&sticker));
    assert!(sticker.len() <= transcode::limit(true));
    assert!(vp8x_alpha(&sticker), "VP8X alpha flag is unset");

    // The canvas must stay transparent rather than being composited onto black.
    // libwebp keeps the alpha plane only when a frame contains some, so the
    // transparent pixels are lifted to alpha 1 — invisible, but present.
    let (min, max) = decoded_alpha_range(&sticker);
    assert_eq!(min, 1, "sticker has no near-transparent pixels");
    assert!(max > 0, "sticker is fully transparent");
}

/// The VP8X alpha bit tells a decoder the animation has transparency; without
/// it a viewer composites the frames onto the background colour (black).
fn vp8x_alpha(sticker: &[u8]) -> bool {
    let start = sticker.windows(4).position(|w| w == b"VP8X").unwrap();
    sticker[start + 8] & 0x10 != 0
}

#[tokio::test]
async fn moving_lottie_stickers_stay_transparent_and_animate() {
    let sticker = lottie::to_sticker(&pulsing_rectangle()).await.unwrap();

    assert!(transcode::is_animated(&sticker).await);
    assert!(vp8x_alpha(&sticker), "moving sticker lost its transparency");
    let (min, max) = decoded_alpha_range(&sticker);
    assert_eq!(min, 1);
    assert!(max > 0);
}

#[tokio::test]
async fn a_non_tgs_file_is_rejected() {
    let error = lottie::to_sticker(b"not a TGS sticker")
        .await
        .unwrap_err()
        .to_string();

    assert!(error.contains("gzip-compressed"), "{error}");
}
