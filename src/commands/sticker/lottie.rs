//! Rendering Telegram's gzipped Lottie stickers to transparent WebP.

use std::io::{Cursor, Read};

use flate2::read::GzDecoder;
use image::RgbaImage;
use thorvg::{ColorSpace, EngineOption, Paint, Thorvg};
use whatsapp_rust::anyhow::{Result, anyhow, bail};

use super::transcode;

const MAX_TGS_BYTES: usize = 2 * 1024 * 1024;
const MAX_JSON_BYTES: usize = 16 * 1024 * 1024;
const MAX_SIDE: u32 = 512;
const MAX_DURATION_SECONDS: f32 = 3.0;
const OUTPUT_FPS: f32 = 30.0;

// ThorVG owns process-global engine state, while pack conversion renders several
// stickers concurrently. Serialising the engine lifetime keeps those instances
// from terminating each other between frames.
static RENDER_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Converts a Telegram `.tgs` animation to an alpha-preserving animated WebP.
pub async fn to_sticker(data: &[u8]) -> Result<Vec<u8>> {
    let data = data.to_vec();
    tokio::task::spawn_blocking(move || {
        let frames = render_frames(&data)?;
        encode_frames(&frames).map_err(|error| anyhow!("Failed to encode Lottie sticker: {error}"))
    })
    .await
    .map_err(|error| anyhow!("Lottie renderer task failed: {error}"))?
}

/// Encodes rendered frames as an animated sticker WebP, spending quality only
/// when the first encoding would not fit WhatsApp's size limit.
///
/// ffmpeg's `libwebp_anim` is not used: it drops the alpha plane outright for
/// many frame sequences, which flattens a transparent sticker onto black.
fn encode_frames(frames: &[RgbaImage]) -> Result<Vec<u8>, String> {
    let (width, height) = (frames[0].width(), frames[0].height());
    let frame_ms = (1000.0 / OUTPUT_FPS) as i32;

    // Frames whose shape fills its own bounding box make libwebp drop the alpha
    // plane; retry those with the padding nudged off zero. Every other sticker
    // keeps its exact pixels and the smaller trimmed-frame encoding.
    let plain: Vec<&[u8]> = frames
        .iter()
        .map(|frame| frame.as_raw().as_slice())
        .collect();
    let has_transparency = frames.iter().any(frame_has_transparency);

    let mut last = Vec::new();
    for &quality in [80.0, 55.0, 30.0].iter() {
        let mut sticker = encode_pass(&plain, width, height, frame_ms, quality)
            .map_err(|error| format!("Could not encode the WebP animation: {error}"))?;
        if has_transparency && !has_alpha_plane(&sticker) {
            let kept: Vec<Vec<u8>> = frames.iter().map(keep_alpha_plane).collect();
            let kept: Vec<&[u8]> = kept.iter().map(Vec::as_slice).collect();
            sticker = encode_pass(&kept, width, height, frame_ms, quality)
                .map_err(|error| format!("Could not encode the WebP animation: {error}"))?;
        }
        if sticker.len() <= transcode::limit(true) {
            return Ok(sticker);
        }
        last = sticker;
    }

    Err(format!(
        "That Lottie animation does not fit WhatsApp's 500 KB limit even after reducing \
         quality ({} KB).",
        last.len() / 1024
    ))
}

/// Runs one libwebp pass over the given frames at the requested quality.
fn encode_pass(
    frames: &[&[u8]],
    width: u32,
    height: u32,
    frame_ms: i32,
    quality: f32,
) -> Result<Vec<u8>, webp_animation::Error> {
    let mut encoder = webp_animation::Encoder::new_with_options(
        (width, height),
        webp_animation::EncoderOptions {
            encoding_config: Some(webp_animation::EncodingConfig::new_lossy(quality)),
            ..Default::default()
        },
    )?;

    for (index, frame) in frames.iter().enumerate() {
        encoder.add_frame(frame, index as i32 * frame_ms)?;
    }

    Ok(encoder.finalize(frames.len() as i32 * frame_ms)?.to_vec())
}

/// Whether the encoded animation carries a transparency plane.
///
/// libwebp writes the VP8X alpha flag when any frame contains alpha, and omits
/// it — along with the alpha data — when the frames it kept are opaque. Without
/// the flag a viewer composites the sticker onto the background, which is black.
fn has_alpha_plane(sticker: &[u8]) -> bool {
    sticker
        .windows(4)
        .position(|window| window == b"VP8X")
        .is_some_and(|start| sticker[start + 8] & 0x10 != 0)
}

/// Whether any pixel of the frame is not fully opaque.
fn frame_has_transparency(frame: &RgbaImage) -> bool {
    frame
        .as_raw()
        .as_chunks::<4>()
        .0
        .iter()
        .any(|pixel| pixel[3] != 255)
}

/// Returns the frame's RGBA bytes with fully transparent pixels lifted to the
/// smallest non-zero alpha.
///
/// libwebp's animation encoder trims each frame to the bounding box of pixels
/// that changed since the previous one, and only keeps an alpha plane when that
/// box holds some transparency. A shape that fills its own bounding box — an
/// opaque square, or a circle touching its edges — leaves an opaque box, so the
/// transparent canvas around it is discarded and the sticker turns black.
/// Lifting alpha 0 to 1 keeps the canvas in the frame; at 1/255 the difference
/// is invisible.
fn keep_alpha_plane(frame: &RgbaImage) -> Vec<u8> {
    let mut data = frame.as_raw().clone();
    for pixel in data.as_chunks_mut::<4>().0 {
        if pixel[3] == 0 {
            pixel[3] = 1;
        }
    }
    data
}

fn render_frames(data: &[u8]) -> Result<Vec<RgbaImage>> {
    if data.len() > MAX_TGS_BYTES {
        bail!("Telegram Lottie sticker exceeds the compressed size limit.");
    }
    if !data.starts_with(&[0x1f, 0x8b]) {
        bail!("Telegram Lottie sticker is not gzip-compressed.");
    }

    let mut json = Vec::new();
    GzDecoder::new(Cursor::new(data))
        .take((MAX_JSON_BYTES + 1) as u64)
        .read_to_end(&mut json)
        .map_err(|error| anyhow!("Could not decompress Telegram Lottie sticker: {error}"))?;
    if json.len() > MAX_JSON_BYTES {
        bail!("Telegram Lottie sticker exceeds the decompressed size limit.");
    }

    let metadata: LottieMetadata = serde_json::from_slice(&json)
        .map_err(|error| anyhow!("Could not parse Telegram Lottie sticker: {error}"))?;
    if metadata.w == 0
        || metadata.h == 0
        || metadata.w > MAX_SIDE
        || metadata.h > MAX_SIDE
        || !metadata.fr.is_finite()
        || metadata.fr <= 0.0
        || !metadata.ip.is_finite()
        || !metadata.op.is_finite()
        || metadata.op <= metadata.ip
        || metadata.op - metadata.ip > MAX_DURATION_SECONDS * metadata.fr
    {
        bail!("Telegram Lottie sticker has invalid animation dimensions or timing.");
    }
    let frame_count = ((metadata.op - metadata.ip) / metadata.fr * OUTPUT_FPS).ceil() as usize;

    let _render_lock = RENDER_LOCK
        .lock()
        .map_err(|error| anyhow!("Lottie renderer lock was poisoned: {error}"))?;
    let engine = Thorvg::init(0)
        .map_err(|error| anyhow!("Could not initialize Lottie renderer: {error}"))?;
    let mut animation = engine
        .lottie_animation()
        .map_err(|error| anyhow!("Could not create Lottie animation: {error}"))?;
    animation
        .load_data(&json)
        .map_err(|error| anyhow!("Could not load Telegram Lottie sticker: {error}"))?;
    // Telegram TGS compositions are square, but any aspect is fitted into the
    // 512x512 sticker box and centred the way the ffmpeg path pads.
    let scale = (MAX_SIDE as f32 / metadata.w as f32).min(MAX_SIDE as f32 / metadata.h as f32);
    let width = (metadata.w as f32 * scale).round() as u32;
    let height = (metadata.h as f32 * scale).round() as u32;
    animation
        .set_size(width as f32, height as f32)
        .map_err(|error| anyhow!("Could not size Telegram Lottie sticker: {error}"))?;

    let mut canvas = engine
        .sw_canvas(EngineOption::None)
        .map_err(|error| anyhow!("Could not create Lottie canvas: {error}"))?;
    let mut buffer = vec![0u32; (MAX_SIDE * MAX_SIDE) as usize];
    // SAFETY: `buffer` holds `MAX_SIDE * MAX_SIDE` pixels and outlives the
    // canvas, which lives to the end of this function, so ThorVG's pointer to it
    // stays valid for every draw. The size arguments match its stride and shape.
    //
    // ThorVG writes premultiplied ABGR pixels; the alpha channel is unpacked to
    // straight RGBA for libwebp's animation encoder.
    unsafe {
        canvas.set_target(
            &mut buffer,
            MAX_SIDE,
            MAX_SIDE,
            MAX_SIDE,
            ColorSpace::ABGR8888,
        )
    }
    .map_err(|error| anyhow!("Could not prepare Lottie canvas: {error}"))?;
    let offset = thorvg::Matrix::IDENTITY.translate(
        (MAX_SIDE - width) as f32 / 2.0,
        (MAX_SIDE - height) as f32 / 2.0,
    );

    let mut frames = Vec::with_capacity(frame_count);
    for index in 0..frame_count {
        let frame = metadata.ip + index as f32 * metadata.fr / OUTPUT_FPS;
        // ThorVG reports the frame it is already showing as an error, which the
        // first frame usually is: the animation starts on `ip` before any call.
        match animation.set_frame(frame) {
            Ok(()) | Err(thorvg::Error::InsufficientCondition) => {}
            Err(error) => return Err(anyhow!("Could not render Lottie frame: {error}")),
        }

        // The animation owns one picture that is re-evaluated for the frame set
        // above; a duplicate taken before the frame advances would snapshot the
        // first frame and repeat it. So each frame is duplicated and centred
        // after `set_frame`, then handed to the canvas.
        let mut picture = animation
            .picture()
            .duplicate()
            .ok_or_else(|| anyhow!("Could not duplicate Lottie picture"))?;
        picture
            .set_transform(&offset)
            .map_err(|error| anyhow!("Could not centre Telegram Lottie sticker: {error}"))?;
        canvas
            .clear()
            .map_err(|error| anyhow!("Could not clear Lottie canvas: {error}"))?;
        canvas
            .add(picture)
            .map_err(|error| anyhow!("Could not add Lottie picture to canvas: {error}"))?;
        canvas
            .render()
            .map_err(|error| anyhow!("Could not draw Lottie frame: {error}"))?;

        let mut rgba = RgbaImage::new(MAX_SIDE, MAX_SIDE);
        for (pixel, value) in rgba.pixels_mut().zip(&buffer) {
            let a = (value >> 24) as u8;
            let b = (value >> 16) as u8;
            let g = (value >> 8) as u8;
            let r = *value as u8;
            if a == 0 {
                *pixel = image::Rgba([0, 0, 0, 0]);
            } else {
                *pixel = image::Rgba([
                    (u16::from(r) * 255 / u16::from(a)) as u8,
                    (u16::from(g) * 255 / u16::from(a)) as u8,
                    (u16::from(b) * 255 / u16::from(a)) as u8,
                    a,
                ]);
            }
        }
        frames.push(rgba);
    }

    Ok(frames)
}

#[derive(serde::Deserialize)]
struct LottieMetadata {
    w: u32,
    h: u32,
    fr: f32,
    ip: f32,
    op: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendered_frames_keep_transparent_pixels() {
        let frames = render_frames(&sample_tgs()).unwrap();

        let alphas: Vec<u8> = frames[0].pixels().map(|pixel| pixel[3]).collect();
        assert!(alphas.contains(&0));
        assert_eq!(frames[0].get_pixel(0, 0)[3], 0);
        assert!(frames[0].get_pixel(256, 256)[3] > 0);
        assert_eq!(frames[0].dimensions(), (MAX_SIDE, MAX_SIDE));
    }

    #[test]
    fn rendered_frames_follow_the_animation() {
        let frames = render_frames(&moving_tgs()).unwrap();

        let first = frames[0].as_raw();
        assert!(
            frames.iter().any(|frame| frame.as_raw() != first),
            "every frame is identical, so the animation was not advanced"
        );
    }

    /// A 64x64 Lottie rectangle that moves across the canvas.
    fn moving_tgs() -> Vec<u8> {
        let json = r#"{"v":"5.7.4","fr":30,"ip":0,"op":10,"w":64,"h":64,"layers":[{"ty":4,"ip":0,"op":10,"st":0,"ks":{"o":{"a":0,"k":100},"r":{"a":0,"k":0},"p":{"a":1,"k":[{"t":0,"s":[0,0,0],"e":[32,0,0]},{"t":10,"s":[32,0,0]}]},"a":{"a":0,"k":[0,0,0]},"s":{"a":0,"k":[100,100,100]}},"shapes":[{"ty":"rc","p":{"a":0,"k":[32,32]},"s":{"a":0,"k":[32,32]},"r":{"a":0,"k":0}},{"ty":"fl","c":{"a":0,"k":[1,0,0,1]},"o":{"a":0,"k":100}},{"ty":"tr","p":{"a":0,"k":[0,0]},"a":{"a":0,"k":[0,0]},"s":{"a":0,"k":[100,100]},"r":{"a":0,"k":0},"o":{"a":0,"k":100}}],"ao":0}]}"#;
        gzip(json)
    }

    /// A still 64x64 Lottie rectangle: the corners stay transparent.
    fn sample_tgs() -> Vec<u8> {
        let json = r#"{"v":"5.7.4","fr":30,"ip":0,"op":2,"w":64,"h":64,"layers":[{"ty":4,"ip":0,"op":2,"st":0,"ks":{"o":{"a":0,"k":100},"r":{"a":0,"k":0},"p":{"a":0,"k":[0,0,0]},"a":{"a":0,"k":[0,0,0]},"s":{"a":0,"k":[100,100,100]}},"shapes":[{"ty":"rc","p":{"a":0,"k":[32,32]},"s":{"a":0,"k":[32,32]},"r":{"a":0,"k":0}},{"ty":"fl","c":{"a":0,"k":[1,0,0,1]},"o":{"a":0,"k":100}},{"ty":"tr","p":{"a":0,"k":[0,0]},"a":{"a":0,"k":[0,0]},"s":{"a":0,"k":[100,100]},"r":{"a":0,"k":0},"o":{"a":0,"k":100}}],"ao":0}]}"#;
        gzip(json)
    }

    /// Gzips a Lottie document the way Telegram's `.tgs` carries it.
    fn gzip(json: &str) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut encoder, json.as_bytes()).unwrap();
        encoder.finish().unwrap()
    }
}
