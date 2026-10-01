use std::process::Stdio;

use megumi_whatsapp::commands::download::ytdlp::transcode;

#[tokio::test]
async fn a_vp9_opus_clip_comes_back_as_h264_aac() {
    if !installed("ffmpeg") || !installed("ffprobe") {
        eprintln!("skipping: ffmpeg is not on PATH");
        return;
    }

    let dir = std::env::temp_dir().join(format!("megumi-dl-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir).unwrap();

    let source = dir.join("clip.webm");
    let status = tokio::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x240:rate=15:duration=1",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:duration=1",
            "-c:v",
            "libvpx-vp9",
            "-c:a",
            "libopus",
            "-shortest",
        ])
        .arg(&source)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()
        .await
        .unwrap();
    assert!(status.success(), "could not produce a vp9/opus clip");

    let converted = transcode(&source).await.unwrap();
    let report = codecs(&converted).await;
    let _ = std::fs::remove_dir_all(&dir);

    assert!(report.contains("h264"), "{report}");
    assert!(report.contains("aac"), "{report}");
}

async fn codecs(path: &std::path::Path) -> String {
    let probe = tokio::process::Command::new("ffprobe")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-show_entries",
            "stream=codec_name",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .await
        .unwrap();
    String::from_utf8_lossy(&probe.stdout).into_owned()
}

fn installed(tool: &str) -> bool {
    std::process::Command::new(tool)
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}
