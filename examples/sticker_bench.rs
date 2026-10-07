//! A throwaway benchmark for the local sticker conversion pipeline.
//!
//! It fetches a Telegram pack, mirrors `convert_telegram_pack`'s download and
//! concurrency (a semaphore of six over `getFile` + download + convert), times
//! each stage, and dumps the raw media to `bench-data/<slug>/` so conversion can
//! be re-timed without touching the network.
//!
//! `cargo run --release --example sticker_bench -- <slug>...`
//!
//! `BENCH_LIMIT=n` caps stickers per pack (default 12). `BENCH_ITERS=n` repeats
//! the serial conversion pass (default 3). `BENCH_CONVERT_ONLY=1` skips the
//! network and re-times conversion over `bench-data/` only.

use std::sync::Arc;
use std::time::{Duration, Instant};

use megumi_whatsapp::commands::sticker::telegram::{TgSticker, fetch_sticker_set};
use megumi_whatsapp::commands::sticker::{lottie, transcode};
use serde::Deserialize;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

const TIMEOUT: Duration = Duration::from_secs(30);
const CONCURRENCY: usize = 6;

#[derive(Debug, Deserialize)]
struct TgResponse<T> {
    ok: bool,
    result: Option<T>,
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TgFile {
    file_path: Option<String>,
}

#[derive(Debug, Clone)]
struct Row {
    index: usize,
    kind: &'static str,
    in_bytes: usize,
    out_bytes: Option<usize>,
    getfile: Duration,
    download: Duration,
    convert: Duration,
    error: Option<String>,
}

fn kind_of(data: &[u8]) -> &'static str {
    if data.starts_with(&[0x1f, 0x8b]) {
        "lottie"
    } else if data.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        "webm"
    } else if transcode::is_webp(data) {
        "webp"
    } else if data.starts_with(b"GIF8") {
        "gif"
    } else {
        "other"
    }
}

fn kind_from_path(path: &str) -> &'static str {
    if path.ends_with(".lottie") {
        "lottie"
    } else if path.ends_with(".webm") {
        "webm"
    } else if path.ends_with(".webp") {
        "webp"
    } else if path.ends_with(".gif") {
        "gif"
    } else {
        "other"
    }
}

/// The dispatch `prepare_sticker_data` performs, over the public API.
async fn convert(data: &[u8], is_video: bool) -> Result<Vec<u8>, String> {
    if data.starts_with(&[0x1f, 0x8b]) {
        return lottie::to_sticker(data).await.map_err(|e| e.to_string());
    }
    let animated = is_video || transcode::is_animated(data).await;
    transcode::to_sticker(data, animated).await
}

/// Whether a dumped file should be treated as video on the way back in.
fn is_video_path(path: &str) -> bool {
    path.ends_with(".webm")
}

async fn get_file_path(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    file_id: &str,
) -> Result<String, String> {
    let url = format!("{base}/bot{token}/getFile?file_id={file_id}");
    let response: TgResponse<TgFile> = http
        .get(&url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    if !response.ok {
        return Err(format!("getFile refused: {:?}", response.description));
    }
    response
        .result
        .and_then(|file| file.file_path)
        .ok_or_else(|| "no file_path".to_string())
}

async fn download_file(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    file_path: &str,
) -> Result<Vec<u8>, String> {
    let url = format!("{base}/file/bot{token}/{file_path}");
    let response = http.get(&url).send().await.map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("download HTTP {}", response.status()));
    }
    Ok(response.bytes().await.map_err(|e| e.to_string())?.to_vec())
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn median(mut values: Vec<Duration>) -> Duration {
    values.sort();
    values[values.len() / 2]
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    let limit: usize = std::env::var("BENCH_LIMIT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(12);
    let iters: usize = std::env::var("BENCH_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    let convert_only = std::env::var("BENCH_CONVERT_ONLY").is_ok();

    let slugs: Vec<String> = std::env::args().skip(1).collect();
    if slugs.is_empty() {
        eprintln!("usage: sticker_bench <slug>...");
        std::process::exit(1);
    }

    let http: &'static reqwest::Client = Box::leak(Box::new(
        reqwest::Client::builder()
            .timeout(TIMEOUT)
            .build()
            .expect("http client"),
    ));

    let (token, base) = if convert_only {
        (String::new(), String::new())
    } else {
        dotenvy::dotenv().ok();
        let token = std::env::var("TELEGRAM_BOT_TOKEN")
            .map(|t| t.trim().to_string())
            .expect("TELEGRAM_BOT_TOKEN must be set");
        let base = std::env::var("TELEGRAM_API_BASE")
            .unwrap_or_else(|_| "https://api.telegram.org".to_string())
            .trim_end_matches('/')
            .to_string();
        (token, base)
    };

    for slug in &slugs {
        println!("\n===== pack {slug} =====");
        let dir = format!("bench-data/{slug}");

        if !convert_only {
            let fetch_start = Instant::now();
            let set = match fetch_sticker_set(http, &base, &token, slug).await {
                Ok(set) => set,
                Err(error) => {
                    println!("FETCH_FAILED {slug}: {error}");
                    continue;
                }
            };
            let fetch = fetch_start.elapsed();
            println!(
                "META slug={} name={} title={:?} animated={} video={} stickers={} fetch_ms={:.0}",
                slug,
                set.name,
                set.title,
                set.is_animated,
                set.is_video,
                set.stickers.len(),
                ms(fetch),
            );

            let stickers: Vec<TgSticker> = set.stickers.iter().take(limit).cloned().collect();
            std::fs::create_dir_all(&dir).expect("create bench-data dir");

            let (rows, wall) = run_pack(
                http,
                &base,
                &token,
                &stickers,
                &dir,
                Arc::new(Semaphore::new(CONCURRENCY)),
            )
            .await;

            for row in &rows {
                println!(
                    "STICKER pack={} idx={} kind={} in={} out={} getfile_ms={:.0} download_ms={:.0} convert_ms={:.0} err={}",
                    slug,
                    row.index,
                    row.kind,
                    row.in_bytes,
                    row.out_bytes
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "-".into()),
                    ms(row.getfile),
                    ms(row.download),
                    ms(row.convert),
                    row.error.as_deref().unwrap_or("-"),
                );
            }

            let sum_convert: Duration = rows.iter().map(|r| r.convert).sum();
            let sum_download: Duration = rows.iter().map(|r| r.download).sum();
            let sum_getfile: Duration = rows.iter().map(|r| r.getfile).sum();
            let converted = rows.iter().filter(|r| r.out_bytes.is_some()).count();
            println!(
                "PACK slug={} wall_ms={:.0} fetch_ms={:.0} n={} converted={} sum_convert_ms={:.0} sum_download_ms={:.0} sum_getfile_ms={:.0}",
                slug,
                ms(wall + fetch),
                ms(fetch),
                rows.len(),
                converted,
                ms(sum_convert),
                ms(sum_download),
                ms(sum_getfile),
            );
        }

        // Serial conversion over the dumped media: the per-sticker cost with no
        // other sticker competing for the CPU, repeated for a stable number.
        let mut paths: Vec<String> = std::fs::read_dir(&dir)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.path().to_string_lossy().to_string())
                    .collect()
            })
            .unwrap_or_default();
        paths.sort();
        for path in &paths {
            let data = std::fs::read(path).expect("read dump");
            let kind = kind_from_path(path);
            let index = path.rsplit('/').next().unwrap_or("?").to_string();
            let mut durations = Vec::with_capacity(iters);
            let mut out_bytes = 0;
            let mut error = None;
            for _ in 0..iters {
                let start = Instant::now();
                match convert(&data, is_video_path(path)).await {
                    Ok(out) => {
                        out_bytes = out.len();
                        durations.push(start.elapsed());
                    }
                    Err(e) => {
                        error = Some(e);
                        break;
                    }
                }
            }
            if durations.is_empty() {
                println!(
                    "CONVERT pack={} file={} kind={} in={} err={}",
                    slug,
                    index,
                    kind,
                    data.len(),
                    error.as_deref().unwrap_or("?"),
                );
                continue;
            }
            let min = *durations.iter().min().unwrap();
            let med = median(durations.clone());
            println!(
                "CONVERT pack={} file={} kind={} in={} out={} iters={} min_ms={:.0} med_ms={:.0}",
                slug,
                index,
                kind,
                data.len(),
                out_bytes,
                durations.len(),
                ms(min),
                ms(med),
            );
        }
    }
}

async fn run_pack(
    http: &'static reqwest::Client,
    base: &str,
    token: &str,
    stickers: &[TgSticker],
    dir: &str,
    semaphore: Arc<Semaphore>,
) -> (Vec<Row>, Duration) {
    let start = Instant::now();
    let mut tasks = JoinSet::new();
    for (index, sticker) in stickers.iter().enumerate() {
        let base = base.to_string();
        let token = token.to_string();
        let dir = dir.to_string();
        let sticker = sticker.clone();
        let semaphore = semaphore.clone();
        tasks.spawn(async move {
            let _permit = semaphore.acquire_owned().await.expect("semaphore");
            let getfile_start = Instant::now();
            let path = match get_file_path(http, &base, &token, &sticker.file_id).await {
                Ok(path) => path,
                Err(error) => {
                    return Row {
                        index,
                        kind: "?",
                        in_bytes: 0,
                        out_bytes: None,
                        getfile: getfile_start.elapsed(),
                        download: Duration::ZERO,
                        convert: Duration::ZERO,
                        error: Some(error),
                    };
                }
            };
            let getfile = getfile_start.elapsed();

            let download_start = Instant::now();
            let data = match download_file(http, &base, &token, &path).await {
                Ok(data) => data,
                Err(error) => {
                    return Row {
                        index,
                        kind: "?",
                        in_bytes: 0,
                        out_bytes: None,
                        getfile,
                        download: download_start.elapsed(),
                        convert: Duration::ZERO,
                        error: Some(error),
                    };
                }
            };
            let download = download_start.elapsed();
            let kind = kind_of(&data);
            let in_bytes = data.len();

            // Keep the original so conversion can be re-timed without the network.
            let _ = std::fs::write(format!("{dir}/{index:03}.{kind}"), &data);

            let convert_start = Instant::now();
            let (out_bytes, error) = match convert(&data, sticker.is_video).await {
                Ok(sticker) => (Some(sticker.len()), None),
                Err(error) => (None, Some(error)),
            };
            let convert = convert_start.elapsed();

            Row {
                index,
                kind,
                in_bytes,
                out_bytes,
                getfile,
                download,
                convert,
                error,
            }
        });
    }

    let mut rows: Vec<Option<Row>> = (0..stickers.len()).map(|_| None).collect();
    while let Some(result) = tasks.join_next().await {
        if let Ok(row) = result {
            let index = row.index;
            rows[index] = Some(row);
        }
    }
    (rows.into_iter().flatten().collect(), start.elapsed())
}
