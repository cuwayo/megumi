use std::process::Stdio;

use megumi_whatsapp::commands::sticker::transcode;
use webp_animation::Decoder;

/// Telegram's video stickers are VP9 WebM with transparency in the container's
/// alpha plane. ffmpeg's built-in VP9 decoder ignores that plane, so the sticker
/// decodes opaque and lands on a black background unless the libvpx decoder is
/// selected — which is what this checks, end to end.
#[tokio::test]
async fn a_transparent_webm_sticker_keeps_its_alpha() {
    if !ffmpeg_supports_libvpx() {
        eprintln!("skipping: ffmpeg is not on PATH or lacks libvpx-vp9");
        return;
    }

    let webm = transparent_webm().await;
    let sticker = transcode::to_sticker(&webm, true).await.unwrap();

    assert!(transcode::is_webp(&sticker));
    let decoder = Decoder::new(&sticker).unwrap();
    let mut min = 255u8;
    let mut max = 0u8;
    for frame in decoder {
        for pixel in frame.data().as_chunks::<4>().0 {
            min = min.min(pixel[3]);
            max = max.max(pixel[3]);
        }
    }
    assert_eq!(
        min, 0,
        "sticker lost its transparency to a black background"
    );
    assert!(max > 0, "sticker is fully transparent");
}

/// A short VP9 WebM whose corners are transparent, made by ffmpeg the way a
/// Telegram video sticker carries its alpha plane.
async fn transparent_webm() -> Vec<u8> {
    let mut child = tokio::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=c=black:s=64x64:d=0.3:r=15",
            "-vf",
            "format=rgba,geq=r='255':g='0':b='0':a='if(between(X,16,47)*between(Y,16,47),255,0)'",
            "-c:v",
            "libvpx-vp9",
            "-pix_fmt",
            "yuva420p",
            "-auto-alt-ref",
            "0",
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

fn ffmpeg_supports_libvpx() -> bool {
    let output = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-decoders"])
        .output();
    output.is_ok_and(|output| String::from_utf8_lossy(&output.stdout).contains("libvpx-vp9"))
}
