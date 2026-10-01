//! Driving `yt-dlp`: the address a command accepts, and one download.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// What one download produced.
pub struct Video {
    /// The media bytes.
    pub bytes: Vec<u8>,
    /// The title `yt-dlp` reported, when it reported one.
    pub title: Option<String>,
    /// The container the file was saved with, such as `mp4`.
    pub extension: String,
    /// The length in seconds, when the metadata carried one.
    pub seconds: Option<u32>,
    /// The pixel size, when the metadata carried one.
    pub size: Option<(u32, u32)>,
    /// A small JPEG preview of the video, when one could be made.
    pub thumbnail: Option<Vec<u8>>,
}

impl Video {
    /// The text sent alongside the media.
    pub fn caption(&self) -> String {
        let title = self.title.as_deref().unwrap_or("Downloaded video");
        let size = (self.bytes.len() as f64) / (1024.0 * 1024.0);
        format!("🎬 *{title}*\n📦 {size:.1} MB")
    }

    /// The name the file is offered under when it is sent as a document.
    pub fn file_name(&self) -> String {
        let title = self
            .title
            .as_deref()
            .map(file_safe)
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "video".to_string());
        format!("{title}.{}", self.extension)
    }
}

/// The metadata `yt-dlp` prints once, before the download.
///
/// This is not where the file ends up: merging changes the name, and only the
/// `after_move` stage knows the final one. That path is printed separately.
#[derive(Deserialize)]
struct Info {
    title: Option<String>,
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    height: Option<u32>,
    /// The address of the thumbnail the site offers, when it offers one.
    #[serde(default)]
    thumbnail: Option<String>,
}

/// How long a download may run before it is given up on.
const TIMEOUT: Duration = Duration::from_secs(600);

/// The largest video that will be downloaded.
///
/// `yt-dlp` refuses anything bigger before fetching it, which keeps one link
/// from filling the disk. The check is on the download size, so a video and
/// audio stream that only cross the limit once merged still gets through.
pub const MAX_DOWNLOAD: u64 = 100 * 1024 * 1024;

/// Downloads the video at `url` into a fresh directory and reads it back.
///
/// `yt-dlp` chooses the best video and audio available and merges them into one
/// mp4. The directory is removed however the download ends, and a download that
/// outlives [`TIMEOUT`] is stopped so it cannot keep writing afterwards.
pub async fn download(url: &str) -> Result<Video, String> {
    let directory = temp_dir()?;

    let outcome = fetch(url, &directory).await;
    let _ = tokio::fs::remove_dir_all(&directory).await;
    outcome
}

/// The first word typed after the command, which is where a URL goes.
pub fn first_word(text: &str) -> Option<&str> {
    let word = text.split_whitespace().next()?;
    (!word.is_empty()).then_some(word)
}

/// Only the schemes the downloader is willing to hand to `yt-dlp`.
pub fn address(argument: Option<&str>) -> Option<String> {
    let argument = argument?.trim();
    (argument.starts_with("http://") || argument.starts_with("https://"))
        .then(|| argument.to_string())
}

/// A directory under the system temp dir that nothing else is using.
fn temp_dir() -> Result<PathBuf, String> {
    let directory = std::env::temp_dir().join(format!("megumi-dl-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory)
        .map_err(|error| format!("Could not create a temp directory: {error}"))?;
    Ok(directory)
}

async fn fetch(url: &str, directory: &Path) -> Result<Video, String> {
    let output = directory.join("%(id)s.%(ext)s");

    let mut child = Command::new("yt-dlp")
        .args([
            "--no-playlist",
            // Refuse before the transfer what would not be sent anyway.
            "--max-filesize",
            &format!("{MAX_DOWNLOAD}"),
            // One file, so a site that offers separate streams still yields a
            // single mp4. Prefer the codecs a phone plays, so a site that has
            // them needs no re-encode; anything else is transcoded afterwards.
            "-f",
            "bv*[vcodec^=avc1]+ba[acodec^=mp4a]/bv*+ba/b",
            "--merge-output-format",
            "mp4",
            // Moves the index to the front of the file, which is what lets a
            // phone start playing before the whole video has loaded.
            "--postprocessor-args",
            "merger:-movflags +faststart",
            "--no-progress",
            // The report of the video, printed before anything is downloaded.
            "--print",
            "%()j",
            // Where the file landed after merging, which is the only name that
            // survives post-processing. Printed as a JSON string, so titles
            // with odd characters stay one value.
            "--print",
            "after_move:%(filepath)j",
            "-o",
            &output.to_string_lossy(),
            "--",
            url,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("Could not run yt-dlp, which `!download` needs: {error}"))?;

    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");

    let finished = tokio::time::timeout(TIMEOUT, async {
        tokio::join!(read_text(stdout), read_text(stderr))
    })
    .await;
    // The pipes closing is not what stops yt-dlp, so a download that overran
    // is killed before its directory is removed.
    if finished.is_err() {
        child.start_kill().ok();
    }
    let (output, errors) = finished.map_err(|_| {
        format!(
            "The download timed out after {} minutes.",
            TIMEOUT.as_secs() / 60
        )
    })?;

    let status = child
        .wait()
        .await
        .map_err(|error| format!("Could not wait for yt-dlp: {error}"))?;
    if !status.success() {
        return Err(explain(&errors));
    }

    // Two lines are printed: the video's metadata, then the final path. A
    // playlist would repeat the pair, and the last pair is the video just
    // written — `--no-playlist` keeps it to one anyway.
    let (path, info) =
        saved(&output).ok_or("yt-dlp finished without saying where it saved the video.")?;
    let info: Info = serde_json::from_str(info)
        .map_err(|error| format!("Could not read yt-dlp's report: {error}"))?;

    // The path is only trusted inside the directory this download owns.
    let path = PathBuf::from(&path);
    if !path.starts_with(directory) {
        return Err("yt-dlp saved the video outside its download directory.".to_string());
    }

    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|error| format!("Could not read the downloaded video: {error}"))?;
    if bytes.is_empty() {
        return Err("yt-dlp saved an empty file.".to_string());
    }

    // A phone plays H.264 with AAC in an MP4 and nothing else, so a download in
    // another codec is re-encoded before it is read back. The preview is taken
    // afterwards, from the file that is actually sent.
    let path = transcode(&path).await?;
    let thumbnail = preview(&path, info.thumbnail.as_deref()).await;

    // The container actually sent, which a transcode changes from whatever the
    // site served to mp4.
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .filter(|ext| !ext.is_empty())
        .unwrap_or("mp4")
        .to_string();

    Ok(Video {
        bytes,
        title: info.title.filter(|title| !title.trim().is_empty()),
        extension,
        seconds: info.duration.and_then(whole_seconds),
        size: info
            .width
            .zip(info.height)
            .filter(|&(width, height)| width > 0 && height > 0),
        thumbnail,
    })
}

/// The codecs of the file at `path`.
struct Codecs {
    /// Whether the video is H.264.
    video: bool,
    /// Whether the audio is AAC. A silent video has nothing to convert.
    audio: Option<bool>,
}

/// Rewrites `path` into an MP4 a phone can play, and returns the path to it.
///
/// WhatsApp plays H.264 video with AAC audio and refuses the rest, so a
/// download that arrived as VP9 or Opus shows its preview and then will not
/// play. Streams that are already in the right codec are copied; only the
/// others are re-encoded, and a file that needs nothing is left untouched.
pub async fn transcode(path: &Path) -> Result<PathBuf, String> {
    let codecs = codecs(path).await;
    if codecs.video
        && codecs.audio.unwrap_or(true)
        && path.extension().is_some_and(|ext| ext == "mp4")
    {
        return Ok(path.to_path_buf());
    }

    let output = path.with_extension("playable.mp4");

    let mut arguments = vec![
        "-hide_banner".to_string(),
        "-loglevel".into(),
        "error".into(),
        "-y".into(),
        "-i".into(),
        path.to_string_lossy().into_owned(),
        "-map".into(),
        "0:v:0".into(),
        "-map".into(),
        "0:a:0?".into(),
    ];
    if codecs.video {
        arguments.extend(["-c:v".into(), "copy".into()]);
    } else {
        // The baseline-compatible profile and pixel format every phone decodes.
        arguments.extend([
            "-c:v".into(),
            "libx264".into(),
            "-preset".into(),
            "veryfast".into(),
            "-crf".into(),
            "23".into(),
            "-pix_fmt".into(),
            "yuv420p".into(),
            "-profile:v".into(),
            "high".into(),
            "-level".into(),
            "4.0".into(),
        ]);
    }
    match codecs.audio {
        Some(true) => arguments.extend(["-c:a".into(), "copy".into()]),
        Some(false) => {
            arguments.extend(["-c:a".into(), "aac".into(), "-b:a".into(), "128k".into()])
        }
        None => {}
    }
    arguments.extend([
        "-movflags".into(),
        "+faststart".into(),
        output.to_string_lossy().into_owned(),
    ]);

    let conversion = Command::new("ffmpeg")
        .args(&arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|error| format!("Could not run ffmpeg, which `!download` needs: {error}"))?;

    if !conversion.status.success() {
        let errors = String::from_utf8_lossy(&conversion.stderr);
        let reason = errors
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("ffmpeg failed without saying why.");
        return Err(format!("Could not make that video playable: {reason}"));
    }

    Ok(output)
}

/// Whether the file's streams are already what a phone plays.
///
/// A probe that fails says nothing is compatible, so the file is re-encoded
/// rather than sent in a codec that might not play.
async fn codecs(path: &Path) -> Codecs {
    let probe = Command::new("ffprobe")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-show_entries",
            "stream=codec_type,codec_name",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await;

    let Ok(probe) = probe else {
        return Codecs {
            video: false,
            audio: None,
        };
    };
    if !probe.status.success() {
        return Codecs {
            video: false,
            audio: None,
        };
    }

    let report = String::from_utf8_lossy(&probe.stdout);
    let mut video = false;
    let mut audio = None;
    for line in report.lines() {
        let mut fields = line.split(',');
        let Some(kind) = fields.next() else { continue };
        let codec = fields.next().unwrap_or("");
        match kind {
            "video" => video = codec == "h264",
            "audio" if audio.is_none() => audio = Some(codec == "aac"),
            _ => {}
        }
    }
    Codecs { video, audio }
}

/// The longest edge of the preview WhatsApp shows before the video loads.
///
/// The client only renders a `jpegThumbnail` up to about this size and silently
/// drops anything larger, which reads as a video with no preview at all.
const THUMB_SIDE: u32 = 200;

/// The largest thumbnail that is worth fetching. Anything bigger is dropped and
/// a frame of the video is used instead.
const MAX_THUMBNAIL: usize = 8 * 1024 * 1024;

/// A JPEG preview of the video: the thumbnail the site offers, when it offers
/// one and it can be fetched, and a frame taken from the file otherwise.
/// Nothing here is worth failing the download over.
async fn preview(path: &Path, thumbnail_url: Option<&str>) -> Option<Vec<u8>> {
    if let Some(url) = thumbnail_url
        && let Some(preview) = fetch_thumbnail(url).await
    {
        return Some(preview);
    }
    frame(path).await
}

/// The picture at `url`, scaled down to a JPEG preview.
async fn fetch_thumbnail(url: &str) -> Option<Vec<u8>> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return None;
    }

    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .ok()?
        .get(url)
        .send()
        .await
        .ok()?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_THUMBNAIL as u64)
    {
        return None;
    }

    let bytes = response.bytes().await.ok()?;
    if bytes.len() > MAX_THUMBNAIL {
        return None;
    }
    jpeg(&bytes)
}

/// One frame of the video, taken a second in so it is not the fade-in from black.
/// A clip shorter than that falls back to its first frame.
async fn frame(path: &Path) -> Option<Vec<u8>> {
    for seek in ["1", "0"] {
        if let Some(preview) = frame_at(path, seek).await {
            return Some(preview);
        }
    }
    None
}

async fn frame_at(path: &Path, seek: &str) -> Option<Vec<u8>> {
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-ss", seek, "-i"])
        .arg(path)
        .args([
            "-frames:v",
            "1",
            "-vf",
            &format!("scale={THUMB_SIDE}:-2"),
            "-f",
            "image2pipe",
            "-vcodec",
            "mjpeg",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;

    output
        .status
        .success()
        .then_some(output.stdout)
        .filter(|frame| !frame.is_empty())
        .and_then(|frame| jpeg(&frame))
}

/// `bytes` scaled to fit the preview box and re-encoded as JPEG, or nothing
/// when they are not an image.
fn jpeg(bytes: &[u8]) -> Option<Vec<u8>> {
    let image = image::load_from_memory(bytes)
        .ok()?
        .thumbnail(THUMB_SIDE, THUMB_SIDE);

    let mut encoded = Vec::new();
    image
        .write_to(
            &mut std::io::Cursor::new(&mut encoded),
            image::ImageFormat::Jpeg,
        )
        .ok()?;
    Some(encoded)
}

/// The final path and the metadata line that precedes it.
///
/// `yt-dlp` prints one JSON value per line and escapes any newline inside a
/// value, so a line is a whole value. The path is a JSON string; the metadata
/// is the nearest preceding line that is a JSON object. Lines that are neither
/// — a warning on stderr never reaches here, but a notice on stdout might — are
/// skipped.
fn saved(output: &str) -> Option<(String, &str)> {
    let mut path = None;
    let mut info = None;
    // The metadata most recently printed, waiting for the path that follows it.
    let mut pending = None;
    for line in output.lines() {
        let line = line.trim();
        if line.starts_with('"') {
            // A path belongs to the metadata printed before it. A path that is
            // not valid JSON leaves the previous pair intact.
            if let Ok(found) = serde_json::from_str(line)
                && let Some(metadata) = pending
            {
                path = Some(found);
                info = Some(metadata);
            }
        } else if line.starts_with('{') {
            pending = Some(line);
        }
    }
    Some((path?, info?))
}

/// The length in whole seconds, or nothing when it would not fit the field.
fn whole_seconds(duration: f64) -> Option<u32> {
    duration.is_finite().then_some(duration.round() as u32)
}

/// What to tell the chat when `yt-dlp` failed: its own last line, which is the
/// one that says why.
fn explain(stderr: &str) -> String {
    let reason = stderr
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("yt-dlp failed without saying why.");
    // Drop the "ERROR: " prefix yt-dlp puts on the line that matters.
    let reason = reason.trim().trim_start_matches("ERROR:").trim();
    format!("Could not download that video: {reason}")
}

/// A title turned into something safe to offer as a file name.
fn file_safe(title: &str) -> String {
    title
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect::<String>()
        .trim()
        .trim_matches('.')
        .chars()
        .take(120)
        .collect()
}

async fn read_text(mut pipe: impl AsyncReadExt + Unpin) -> String {
    let mut text = String::new();
    let _ = pipe.read_to_string(&mut text).await;
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_http_addresses_are_accepted() {
        assert_eq!(
            address(Some("https://youtu.be/abc")).as_deref(),
            Some("https://youtu.be/abc")
        );
        assert_eq!(
            address(Some("  http://example.com/v  ")).as_deref(),
            Some("http://example.com/v")
        );
        assert_eq!(address(Some("ftp://example.com/v")), None);
        assert_eq!(address(Some("not a url")), None);
        assert_eq!(address(None), None);
    }

    #[test]
    fn the_first_word_is_the_address() {
        assert_eq!(
            first_word("https://youtu.be/abc please"),
            Some("https://youtu.be/abc")
        );
        assert_eq!(first_word("   "), None);
        assert_eq!(first_word(""), None);
    }

    #[test]
    fn a_failure_is_explained_by_yt_dlps_own_last_line() {
        let stderr = "\
[youtube] Extracting URL: https://youtu.be/abc\n\
ERROR: [youtube] abc: Video unavailable\n";
        assert_eq!(
            explain(stderr),
            "Could not download that video: [youtube] abc: Video unavailable"
        );
        assert_eq!(
            explain("   \n"),
            "Could not download that video: yt-dlp failed without saying why."
        );
    }

    #[test]
    fn durations_that_are_not_a_length_are_dropped() {
        assert_eq!(whole_seconds(12.4), Some(12));
        assert_eq!(whole_seconds(12.5), Some(13));
        assert_eq!(whole_seconds(f64::NAN), None);
        assert_eq!(whole_seconds(f64::INFINITY), None);
    }

    #[test]
    fn a_title_becomes_a_safe_file_name() {
        let video = Video {
            bytes: vec![0; 2 * 1024 * 1024],
            title: Some("a/b: c?".to_string()),
            extension: "mp4".to_string(),
            seconds: None,
            size: None,
            thumbnail: None,
        };
        assert_eq!(video.file_name(), "a_b_ c_.mp4");
        assert_eq!(video.caption(), "🎬 *a/b: c?*\n📦 2.0 MB");
    }

    #[test]
    fn a_thumbnail_is_scaled_down_to_a_jpeg_preview() {
        let wide = image::RgbImage::from_pixel(1280, 720, image::Rgb([20, 40, 60]));
        let mut png = Vec::new();
        wide.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();

        let preview = jpeg(&png).unwrap();
        let decoded = image::load_from_memory(&preview).unwrap();
        assert!(decoded.width() <= THUMB_SIDE, "{}", decoded.width());
        assert!(decoded.height() <= THUMB_SIDE, "{}", decoded.height());
        assert!(preview.starts_with(&[0xff, 0xd8]), "not a jpeg");
    }

    #[test]
    fn the_saved_file_is_the_last_path_yt_dlp_prints() {
        let output = "\
{\"title\":\"First\",\"ext\":\"mp4\"}\n\
\"/tmp/megumi-dl/first.mp4\"\n\
{\"title\":\"Second\",\"ext\":\"webm\"}\n\
\"/tmp/megumi-dl/second.webm\"\n";
        let (path, info) = saved(output).unwrap();
        assert_eq!(path, "/tmp/megumi-dl/second.webm");
        assert!(info.contains("Second"), "{info}");
    }

    #[test]
    fn a_path_with_quotes_and_newlines_stays_one_value() {
        // yt-dlp escapes everything inside a JSON string, so a path with a
        // quote or a newline stays on the one line it was printed on.
        let path = "/tmp/a \"b\"\nc.mp4";
        let output = format!(
            "{{\"title\":\"x\"}}\n{}\n",
            serde_json::to_string(path).unwrap()
        );
        assert_eq!(saved(&output).unwrap().0, path);
    }

    #[test]
    fn output_without_a_path_is_not_a_download() {
        assert!(saved("{\"title\":\"x\"}\n").is_none());
        assert!(saved("").is_none());
    }

    #[test]
    fn warnings_between_the_records_are_ignored() {
        let output = "\
Deprecated Feature: something odd\n\
{\"title\":\"Clip\",\"ext\":\"mp4\"}\n\
\"/tmp/megumi-dl/clip.mp4\"\n";
        let (path, info) = saved(output).unwrap();
        assert_eq!(path, "/tmp/megumi-dl/clip.mp4");
        assert!(info.contains("Clip"), "{info}");
    }

    #[test]
    fn a_video_without_a_title_still_has_a_name() {
        let video = Video {
            bytes: Vec::new(),
            title: Some("   ".to_string()),
            extension: "webm".to_string(),
            seconds: None,
            size: None,
            thumbnail: None,
        };
        // The title is kept as given; only the file name falls back.
        assert_eq!(video.file_name(), "video.webm");
    }
}
