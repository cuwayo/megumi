//! Fitting converted WebP into a sticker WhatsApp accepts.

use std::io::Cursor;
use std::num::NonZeroU8;
use std::process::Stdio;
use std::time::Duration;

use image::ImageReader;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStderr, ChildStdin, ChildStdout, Command};

/// WhatsApp sizes a sticker's box to 512x512: media of any shape is fitted into
/// it and centred, and what is left over is padded with transparency rather than
/// cropped away.
pub const SIDE: u32 = 512;

/// WhatsApp's published sticker sizes, in bytes: a still sticker is capped far
/// tighter than an animated one.
pub const STILL_LIMIT: usize = 100 * 1024;
const ANIMATED_LIMIT: usize = 500 * 1024;

/// Encoding attempts for still stickers: quality steps only.
const STILL_ATTEMPTS: &[(u8, Option<NonZeroU8>)] = &[(80, None), (55, None), (30, None)];

/// Encoding attempts for animated stickers, tried in order until the result
/// fits under the 500 KB limit.
///
/// Each entry is `(quality, fps_cap)`. The fps cap is applied only when the
/// source frame rate exceeds it, so low-fps GIFs are never up-sampled.
/// Reducing the frame rate is far cheaper than a quality drop alone because
/// fewer frames means fewer VP8 blocks to encode and less data in the output.
/// Quality is stepped down only after exhausting the fps options at that tier.
///
/// A sticker that carries real transparency spends bytes on its alpha plane, so
/// the ladder reaches low frame rates: a three-second Telegram video sticker
/// whose alpha fits no other way still comes out animated, if choppier.
const ANIMATED_ATTEMPTS: &[(u8, Option<NonZeroU8>)] = &[
    (80, None),
    (80, nzu8(20)),
    (70, nzu8(15)),
    (60, nzu8(12)),
    (55, nzu8(10)),
    (40, nzu8(8)),
    (30, nzu8(6)),
    (25, nzu8(5)),
];

const TIMEOUT: Duration = Duration::from_secs(60);

/// A `const`-friendly constructor for `Some(NonZeroU8)` used in the attempt tables.
const fn nzu8(n: u8) -> Option<NonZeroU8> {
    NonZeroU8::new(n)
}

/// Fits `webp` into a 512x512 sticker, spending quality and frame rate only
/// when the first encoding would not fit WhatsApp's size limit.
///
/// Despite the name, `webp` accepts whatever ffmpeg can read: the media the chat
/// handed over is converted here in one pass. Media that already is a WebP of the
/// sticker's own size and under its limit is the finished sticker, so it is
/// answered as it stands rather than re-encoded.
///
/// An animated sticker is encoded at the source frame rate first, since that is
/// the best quality. When it does not fit, the overshoot says how much of the
/// frame rate to give up and the next pass jumps straight to that rate; walking
/// every rung in between is most of what made this slow. The full ladder stays
/// as the fallback for media whose frame rate cannot be read or whose size does
/// not fall the way the estimate assumes.
pub async fn to_sticker(webp: &[u8], animated: bool) -> Result<Vec<u8>, String> {
    let limit = limit(animated);
    if is_webp(webp) && webp.len() <= limit && dimensions(webp) == Some((SIDE, SIDE)) {
        return Ok(webp.to_vec());
    }

    if !animated {
        for &(quality, _) in STILL_ATTEMPTS {
            let sticker = encode(webp, false, quality, None).await?;
            if sticker.len() <= limit {
                return Ok(sticker);
            }
        }
    } else {
        let full = encode(webp, true, 80, None).await?;
        if full.len() <= limit {
            return Ok(full);
        }

        if let Some(cap) = fitting_fps(webp, full.len(), limit).await {
            let sticker = encode(webp, true, 80, Some(cap)).await?;
            if sticker.len() <= limit {
                return Ok(sticker);
            }
        }

        // The first rung was the pass already made, so the fallback resumes
        // after it.
        for &(quality, fps_cap) in ANIMATED_ATTEMPTS.iter().skip(1) {
            let sticker = encode(webp, true, quality, fps_cap).await?;
            if sticker.len() <= limit {
                return Ok(sticker);
            }
        }
    }

    Err(format!(
        "That media does not fit WhatsApp's {} KB limit for {} stickers even after \
         reducing quality and frame rate.",
        limit / 1024,
        if animated { "animated" } else { "still" }
    ))
}

/// The frame rate a first encode's overshoot implies will fit, or `None` when
/// the source's frame rate cannot be read.
///
/// An animated WebP's size tracks its frame count, so a sticker that came out
/// `size` bytes at the source's `fps` needs about `fps * limit / size` to fit.
/// The estimate is held a little under that so it does not land just over the
/// line and buy another pass.
async fn fitting_fps(data: &[u8], size: usize, limit: usize) -> Option<NonZeroU8> {
    let fps = source_fps(data).await?;
    let cap = (fps * limit as f32 / size as f32 * 0.9).floor();
    if cap < 1.0 {
        return None;
    }
    NonZeroU8::new(cap.min(f32::from(u8::MAX)) as u8)
}

/// The frame rate ffmpeg reports for this media's first video stream, read from
/// the container rather than by decoding.
async fn source_fps(data: &[u8]) -> Option<f32> {
    let text = probe(
        data,
        &[
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=r_frame_rate",
            "-of",
            "default=nokey=1:noprint_wrappers=1",
        ],
    )
    .await?;
    let text = String::from_utf8_lossy(&text);
    let (numerator, denominator) = text.trim().split_once('/')?;
    let fps = numerator.parse::<f32>().ok()? / denominator.parse::<f32>().ok()?;
    fps.is_finite().then_some(fps).filter(|fps| *fps > 0.0)
}

pub fn limit(animated: bool) -> usize {
    if animated {
        ANIMATED_LIMIT
    } else {
        STILL_LIMIT
    }
}

/// The decoder ffmpeg has to be told to use for media whose transparency is
/// carried in a Matroska/WebM alpha plane.
///
/// ffmpeg's built-in VP8/VP9 decoders ignore that plane, so a video sticker
/// decodes fully opaque and lands on a black background; the libvpx decoders
/// read it. Only WebM is named here — other containers do not store alpha this
/// way, and forcing a decoder on media that is not VP8/VP9 would fail.
fn alpha_decoder(data: &[u8]) -> Option<&'static str> {
    // The EBML header that opens every Matroska/WebM file.
    if !data.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return None;
    }
    // CodecID appears in the Tracks element near the head of the file; a
    // bounded scan keeps a large video from costing a full pass.
    let head = &data[..data.len().min(64 * 1024)];
    if head.windows(5).any(|window| window == b"V_VP9") {
        Some("libvpx-vp9")
    } else if head.windows(5).any(|window| window == b"V_VP8") {
        Some("libvpx")
    } else {
        None
    }
}

/// Runs one ffmpeg pass: fit to the sticker box, then encode it as WebP.
///
/// The still and animated encoders are not interchangeable: the animated one
/// wraps even a single frame in an animation container, and that container is
/// what [`moves`] and WhatsApp read as "this sticker moves".
async fn encode(
    webp: &[u8],
    animated: bool,
    quality: u8,
    fps_cap: Option<NonZeroU8>,
) -> Result<Vec<u8>, String> {
    let filter = filter(fps_cap);
    let quality = quality.to_string();
    let codec = if animated { "libwebp_anim" } else { "libwebp" };
    let mut tail = vec![
        "-i", "pipe:0",
        // Strip audio — stickers are video-only; without this some containers
        // cause ffmpeg to wait for an audio track that never ends.
        "-an", "-vf", &filter, "-c:v", codec, "-quality", &quality,
    ];
    if animated {
        // vfr avoids duplicating frames that the fps filter already dropped,
        // keeping the WebP frame timestamps faithful to the intended cadence.
        tail.extend_from_slice(&["-fps_mode", "vfr", "-loop", "0"]);
    }
    tail.extend_from_slice(&["-f", "webp", "pipe:1"]);

    // Telegram stores a video sticker's transparency in the container's alpha
    // plane, which ffmpeg's built-in VP8/VP9 decoders ignore; the libvpx ones
    // read it. If that decoder is not built in, fall back to the default rather
    // than refusing the sticker — it just comes back opaque as it did before.
    if let Some(decoder) = alpha_decoder(webp) {
        let mut arguments = vec!["-hide_banner", "-loglevel", "error", "-c:v", decoder];
        arguments.extend_from_slice(&tail);
        match run_ffmpeg(&arguments, webp).await {
            Ok(sticker) => return Ok(sticker),
            // Only a build without libvpx warrants the second run; any other
            // failure (a bad decode, a timeout) is the real answer and must not
            // buy a retry that spends the whole timeout again.
            Err(error) if error.contains("Unknown decoder") => {}
            Err(error) => return Err(error),
        }
    }

    let mut arguments = vec!["-hide_banner", "-loglevel", "error"];
    arguments.extend_from_slice(&tail);
    run_ffmpeg(&arguments, webp).await
}

/// Spawns ffmpeg with `arguments`, pipes `input` through it, and answers with
/// its stdout once it exits successfully.
async fn run_ffmpeg(arguments: &[&str], input: &[u8]) -> Result<Vec<u8>, String> {
    let mut command = Command::new("ffmpeg");
    command
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = command
        .spawn()
        .map_err(|error| format!("Could not run ffmpeg, which `!sticker` needs: {error}"))?;
    let (Some(stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return Err("Could not capture ffmpeg's pipes.".to_string());
    };

    let (sticker, errors) = tokio::time::timeout(TIMEOUT, transcode(stdin, stdout, stderr, input))
        .await
        .map_err(|_| format!("ffmpeg timed out after {}s.", TIMEOUT.as_secs()))?;

    // ffmpeg explains itself on stderr, so only the exit status is read here.
    let status = child
        .wait()
        .await
        .map_err(|error| format!("Could not wait for ffmpeg: {error}"))?;
    if !status.success() {
        return Err(format!(
            "ffmpeg could not encode that media: {}",
            errors.trim()
        ));
    }

    Ok(sticker)
}

/// Writes the input and reads the output at the same time, so a body either side
/// of the pipe is too big to buffer never stalls the other.
async fn transcode(
    mut stdin: ChildStdin,
    mut stdout: ChildStdout,
    mut stderr: ChildStderr,
    input: &[u8],
) -> (Vec<u8>, String) {
    let write = async move {
        // Dropping the handle at the end of this block is what closes ffmpeg's
        // input; ffmpeg failing outright surfaces through its exit status.
        let _ = stdin.write_all(input).await;
    };
    let read = async move {
        let mut sticker = Vec::new();
        let _ = stdout.read_to_end(&mut sticker).await;
        sticker
    };
    let errors = async move {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text).await;
        text
    };

    let (_, sticker, errors) = tokio::join!(write, read, errors);
    (sticker, errors)
}

/// Fits any aspect ratio into the square sticker box, centring what it pads.
/// The padding is transparent, which is only kept if the frames reach it with an
/// alpha channel, so opaque sources are widened to RGBA first.
///
/// When `fps_cap` is `Some(n)` an `fps` filter is prepended that drops frames
/// only when the source exceeds `n` fps, so low-fps sources (GIFs, slow
/// stickers) are never up-sampled.
pub fn filter(fps_cap: Option<NonZeroU8>) -> String {
    let scale_pad = format!(
        "format=rgba,\
         scale={SIDE}:{SIDE}:force_original_aspect_ratio=decrease:flags=lanczos,\
         pad={SIDE}:{SIDE}:(ow-iw)/2:(oh-ih)/2:color=0x00000000"
    );
    match fps_cap {
        None => scale_pad,
        Some(cap) => format!("fps='if(gt(source_fps,{cap}),{cap},source_fps)',{scale_pad}"),
    }
}

/// Whether these bytes are a WebP, told from its RIFF container.
pub fn is_webp(data: &[u8]) -> bool {
    data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP"
}

/// Whether this media moves, told from its container before any decoding.
///
/// The WebP-only check [`whatsapp_rust::wacore::webp::is_animated`] answers
/// `false` for every other moving format, which sends a two-second `.webm`
/// Telegram sticker down the still path and against the 100 KB still limit it
/// can never meet. Every container that can carry motion is therefore named
/// here, and only media whose bytes are in none of them is probed with ffprobe.
pub fn moves(data: &[u8]) -> bool {
    if webp_moves(data) {
        return true;
    }
    signature_moves(data)
}

/// Whether an animated WebP moves, told from its RIFF chunks.
///
/// This is [`whatsapp_rust::wacore::webp::is_animated`] again so the answer does
/// not depend on a dependency's private decision; the VP8X animation flag and
/// the `ANIM`/`ANMF` chunks are the two ways a WebP states that it animates.
fn webp_moves(data: &[u8]) -> bool {
    if !is_webp(data) {
        return false;
    }
    let mut offset = 12;
    while offset + 8 <= data.len() {
        let fourcc = &data[offset..offset + 4];
        let size = u32::from_le_bytes([
            data[offset + 4],
            data[offset + 5],
            data[offset + 6],
            data[offset + 7],
        ]) as usize;
        if fourcc == b"ANIM" || fourcc == b"ANMF" {
            return true;
        }
        if fourcc == b"VP8X" && size >= 1 && offset + 8 < data.len() && data[offset + 8] & 0x02 != 0
        {
            return true;
        }
        // Padding keeps chunks word-aligned; each step checked so a corrupt
        // size near `usize::MAX` breaks out rather than wrapping.
        offset = match offset
            .checked_add(8)
            .and_then(|v| v.checked_add(size))
            .and_then(|v| v.checked_add(size & 1))
        {
            Some(next) => next,
            None => break,
        };
    }
    false
}

/// Whether these bytes announce themselves as a container that can hold motion.
///
/// A moving image is named by its magic number alone: GIF's `GIF8` header, an
/// ISO base media file's `ftyp` brand (`isom`, `mp42`, `qt  ` — the MP4 and MOV
/// families), and Matroska/WebM's EBML header. A still image carries a different
/// signature (PNG, JPEG, BMP), so it is not mistaken for its container's cousin.
fn signature_moves(data: &[u8]) -> bool {
    if data.len() < 12 {
        return false;
    }
    // GIF, which is animated by definition: a still GIF is rare enough that the
    // larger allowance it gets costs nothing.
    if &data[..4] == b"GIF8" {
        return true;
    }
    // Matroska and WebM, whose EBML header also names a DocType of `webm`.
    if data[..4] == [0x1A, 0x45, 0xDF, 0xA3] {
        return true;
    }
    // ISO base media (MP4/MOV/3GP): `....ftyp` where the box type sits at byte 4.
    if &data[4..8] == b"ftyp" {
        let brand = &data[8..12];
        return matches!(
            brand,
            b"isom"
                | b"iso2"
                | b"iso4"
                | b"iso5"
                | b"iso6"
                | b"mp41"
                | b"mp42"
                | b"avc1"
                | b"dash"
                | b"qt  "
                | b"MSNV"
                | b"M4V "
                | b"3gp4"
                | b"3gp5"
                | b"3g2a"
        );
    }
    false
}

/// Whether ffmpeg sees more than one frame in this media.
///
/// Used only for media whose signature named no container, so a format this
/// module has never heard of still gets its animated allowance if it moves.
/// A media with no video stream at all — a plain PNG or JPEG — is still.
pub async fn frames_move(data: &[u8]) -> bool {
    let Some(text) = probe(
        data,
        &[
            "-select_streams",
            "v:0",
            "-count_frames",
            "-show_entries",
            "stream=nb_read_frames",
            "-of",
            "default=nokey=1:noprint_wrappers=1",
        ],
    )
    .await
    else {
        return false;
    };

    let count: u64 = String::from_utf8_lossy(&text)
        .split_whitespace()
        .next()
        .and_then(|first| first.parse().ok())
        .unwrap_or(0);
    count > 1
}

/// Runs ffprobe over `data` with `arguments` and answers with its stdout, or
/// `None` if it could not be run or finished in time.
///
/// ffprobe takes the media on stdin, so callers pass the bytes they already
/// hold rather than a path ffmpeg would have to reopen.
async fn probe(data: &[u8], arguments: &[&str]) -> Option<Vec<u8>> {
    let mut command = Command::new("ffprobe");
    command
        .args(["-hide_banner", "-loglevel", "error"])
        .args(arguments)
        .arg("-i")
        .arg("pipe:0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);

    let mut child = command.spawn().ok()?;
    let mut stdin = child.stdin.take()?;
    let mut stdout = child.stdout.take()?;

    let write = async move {
        // Dropping the handle is what closes ffprobe's input.
        let _ = stdin.write_all(data).await;
    };
    let read = async move {
        let mut text = Vec::new();
        let _ = stdout.read_to_end(&mut text).await;
        text
    };
    let (_, text) = tokio::time::timeout(TIMEOUT, async { tokio::join!(write, read) })
        .await
        .ok()?;

    let _ = child.wait().await;
    Some(text)
}

/// Whether this media moves, probing with ffprobe only when its bytes named no
/// container that could. The async sibling of [`moves`], for callers that hold
/// media ffmpeg — not just this module — has to read the format of.
pub async fn is_animated(data: &[u8]) -> bool {
    if moves(data) {
        return true;
    }
    // A WebP, GIF, or named video container already answered above; only media
    // whose signature is unknown is worth an ffprobe process.
    frames_move(data).await
}

/// The pixel size a WebP declares, read from its header. Decoding a frame to ask
/// would cost far more than the answer decides.
pub fn dimensions(webp: &[u8]) -> Option<(u32, u32)> {
    ImageReader::new(Cursor::new(webp))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}
