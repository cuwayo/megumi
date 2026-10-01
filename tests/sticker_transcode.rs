use std::io::Cursor;
use std::num::NonZeroU8;
use std::process::Stdio;

use image::{DynamicImage, ImageFormat, RgbaImage};
use megumi_whatsapp::commands::sticker::transcode::{
    SIDE, STILL_LIMIT, dimensions, filter, is_animated, is_webp, limit, moves, to_sticker,
};

/// A WebP of `side`x`side` pixels, which `image` writes losslessly.
fn webp(side: u32) -> Vec<u8> {
    let image = DynamicImage::ImageRgba8(RgbaImage::new(side, side));
    let mut webp = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut webp), ImageFormat::WebP)
        .unwrap();
    webp
}

#[test]
fn the_filter_fits_and_pads_into_the_sticker_box() {
    assert_eq!(
        filter(None),
        "format=rgba,\
         scale=512:512:force_original_aspect_ratio=decrease:flags=lanczos,\
         pad=512:512:(ow-iw)/2:(oh-ih)/2:color=0x00000000"
    );
}

#[test]
fn the_filter_prepends_an_fps_cap_when_given() {
    assert_eq!(
        filter(NonZeroU8::new(15)),
        "fps='if(gt(source_fps,15),15,source_fps)',\
         format=rgba,\
         scale=512:512:force_original_aspect_ratio=decrease:flags=lanczos,\
         pad=512:512:(ow-iw)/2:(oh-ih)/2:color=0x00000000"
    );
}

#[test]
fn animated_stickers_get_the_larger_allowance() {
    assert_eq!(limit(true), 500 * 1024);
    assert_eq!(limit(false), 100 * 1024);
}

#[test]
fn tells_a_webp_from_other_media() {
    assert!(is_webp(&webp(8)));
    assert!(!is_webp(b"\x89PNG\r\n\x1a\n\r\n\r\n\r\n\r\n"));
    assert!(!is_webp(b"RIFF"));
}

#[test]
fn reads_a_webps_size_from_its_header() {
    assert_eq!(dimensions(&webp(SIDE)), Some((SIDE, SIDE)));
    assert_eq!(dimensions(b"not an image"), None);
}

#[tokio::test]
async fn a_conforming_sticker_is_answered_as_it_stands() {
    let sticker = webp(SIDE);
    assert!(sticker.len() <= STILL_LIMIT);

    let converted = to_sticker(&sticker, false).await.unwrap();

    assert_eq!(converted, sticker);
}

#[tokio::test]
async fn a_webp_of_the_wrong_size_is_fitted_into_the_sticker_box() {
    if !ffmpeg_is_installed() {
        eprintln!("skipping: ffmpeg is not on PATH");
        return;
    }

    let converted = to_sticker(&webp(64), false).await.unwrap();

    assert_eq!(dimensions(&converted), Some((SIDE, SIDE)));
    assert!(converted.len() <= STILL_LIMIT, "{} bytes", converted.len());
    // The still encoder leaves the animation container out, which is what
    // makes `is_animated` answer honestly for a sticker that does not move.
    assert!(!whatsapp_rust::wacore::webp::is_animated(&converted));
}

fn ffmpeg_is_installed() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// A minimal animated WebP: a VP8X chunk whose animation flag is set.
fn animated_webp() -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(b"WEBP");
    buf.extend_from_slice(b"VP8X");
    buf.extend_from_slice(&10u32.to_le_bytes());
    buf.push(0x02);
    buf.extend_from_slice(&[0u8; 9]);
    let riff_size = (buf.len() - 8) as u32;
    buf[4..8].copy_from_slice(&riff_size.to_le_bytes());
    buf
}

#[test]
fn a_still_webp_does_not_move() {
    // `image` writes lossless WebP, which for a single frame carries no VP8X
    // animation flag and no ANIM chunk.
    assert!(!moves(&webp(SIDE)));
}

#[test]
fn an_animated_webp_moves() {
    assert!(moves(&animated_webp()));
}

#[test]
fn unnamed_containers_that_carry_motion_are_named() {
    // GIF, WebM/Matroska, and MP4 are containers that carry motion, so the
    // larger allowance follows from the signature alone. The trailing bytes
    // only need to reach the minimum length the check reads.
    assert!(moves(&[
        b'G', b'I', b'F', b'8', b'9', b'a', 0, 0, 0, 0, 0, 0
    ]));
    assert!(moves(&[0x1A, 0x45, 0xDF, 0xA3, 0, 0, 0, 0, 0, 0, 0, 0]));
    assert!(moves(b"\x00\x00\x00\x18ftypisom\x00\x00\x00\x00isomiso2"));
    assert!(moves(b"\x00\x00\x00\x14ftypqt  \x00\x00\x00\x00qt  "));
}

#[test]
fn a_still_image_is_not_mistaken_for_a_moving_container() {
    // PNG and JPEG share no signature with the moving containers above.
    assert!(!moves(b"\x89PNG\r\n\x1a\n\x00\x00\x00\x00\x00\x00"));
    assert!(!moves(b"\xff\xd8\xff\xe0\x00\x10JFIF\x00\x00\x00\x00"));
    assert!(!moves(b"short"));
}

#[tokio::test]
async fn a_webm_is_read_as_animated_where_a_webp_only_check_is_not() {
    if !ffmpeg_is_installed() {
        eprintln!("skipping: ffmpeg is not on PATH");
        return;
    }
    // A two-frame WebM: the container alone says it moves, so no ffprobe is
    // needed and the animated allowance is granted before any decoding.
    let webm = two_frame_webm().await;
    assert!(moves(&webm));
    assert!(is_animated(&webm).await);
    // The WebP-only helper the bug leaned on answers the opposite.
    assert!(!whatsapp_rust::wacore::webp::is_animated(&webm));
}

/// A tiny two-frame WebM, produced by ffmpeg from a generated source.
async fn two_frame_webm() -> Vec<u8> {
    use tokio::process::Command;

    let mut child = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=c=red:s=64x64:d=0.1:r=10",
            "-c:v",
            "libvpx-vp9",
            "-f",
            "webm",
            "pipe:1",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let mut webm = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut child.stdout.take().unwrap(), &mut webm)
        .await
        .unwrap();
    child.wait().await.unwrap();
    webm
}
