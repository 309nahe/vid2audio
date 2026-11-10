//! vid2audio: a tiny web tool that extracts the audio track from a short video.
//!
//! Stateless by design: each upload goes to a private temp dir, ffmpeg writes the
//! result next to it, the bytes are returned and the dir is deleted. Nothing is kept.

use axum::{
    extract::{DefaultBodyLimit, Multipart},
    http::{header, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Router,
};
use std::{path::Path, process::Stdio, sync::OnceLock, time::Duration};
use tokio::{io::AsyncWriteExt, process::Command, sync::Semaphore, time::timeout};

/// Upload size cap (bytes).
const MAX_UPLOAD: usize = 300 * 1024 * 1024;
/// Longest accepted media: 3 minutes (+0.5 s slack for container rounding).
const MAX_SECS: f64 = 180.5;
/// Hard limit for one ffmpeg run.
const FFMPEG_TIMEOUT: Duration = Duration::from_secs(120);

/// Limits concurrent conversions; extra requests get 429.
static SLOTS: OnceLock<Semaphore> = OnceLock::new();

type Fail = (StatusCode, String);
fn fail(code: StatusCode, msg: impl Into<String>) -> Fail {
    (code, msg.into())
}

/// (extension, mime, ffmpeg codec args)
fn format_args(fmt: &str) -> Option<(&'static str, &'static str, &'static [&'static str])> {
    Some(match fmt {
        "mp3" => ("mp3", "audio/mpeg", &["-c:a", "libmp3lame", "-q:a", "2"]),
        "m4a" => ("m4a", "audio/mp4", &["-c:a", "aac", "-b:a", "192k"]),
        "opus" => ("opus", "audio/ogg", &["-c:a", "libopus", "-b:a", "128k"]),
        "wav" => ("wav", "audio/wav", &["-c:a", "pcm_s16le"]),
        "flac" => ("flac", "audio/flac", &["-c:a", "flac"]),
        _ => return None,
    })
}

/// Media duration in seconds via ffprobe; also serves as the "is this media?" check.
async fn duration(path: &Path) -> Result<f64, Fail> {
    let o = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0"])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("ffprobe: {e}")))?;
    String::from_utf8_lossy(&o.stdout)
        .trim()
        .parse::<f64>()
        .map_err(|_| fail(StatusCode::UNPROCESSABLE_ENTITY, "Not a readable media file"))
}

/// POST /convert: multipart fields `format` (mp3|m4a|opus|wav|flac) and `file`.
async fn convert(mut mp: Multipart) -> Result<Response, Fail> {
    let _slot = SLOTS
        .get()
        .unwrap()
        .try_acquire()
        .map_err(|_| fail(StatusCode::TOO_MANY_REQUESTS, "Busy, try again in a moment"))?;
    // Private temp dir, removed on drop (also on early return / error).
    let dir = tempfile::tempdir().map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let input = dir.path().join("in");
    let mut fmt = String::from("mp3");
    let mut stem = String::from("audio");
    let mut got = false;

    while let Some(mut field) = mp.next_field().await.map_err(|e| fail(StatusCode::BAD_REQUEST, e.to_string()))? {
        match field.name() {
            Some("format") => fmt = field.text().await.unwrap_or_default(),
            Some("file") => {
                if let Some(n) = field.file_name() {
                    let s = Path::new(n).file_stem().and_then(|s| s.to_str()).unwrap_or("audio");
                    stem = s.chars().filter(|c| c.is_alphanumeric() || "-_ .".contains(*c)).take(80).collect();
                    if stem.trim().is_empty() {
                        stem = "audio".into();
                    }
                }
                let mut f = tokio::fs::File::create(&input).await.map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
                while let Some(chunk) = field.chunk().await.map_err(|e| fail(StatusCode::BAD_REQUEST, e.to_string()))? {
                    f.write_all(&chunk).await.map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
                }
                f.flush().await.ok();
                got = true;
            }
            _ => {}
        }
    }
    if !got {
        return Err(fail(StatusCode::BAD_REQUEST, "No file uploaded"));
    }
    let (ext, mime, codec) = format_args(&fmt).ok_or_else(|| fail(StatusCode::BAD_REQUEST, "Unknown format"))?;

    if duration(&input).await? > MAX_SECS {
        return Err(fail(StatusCode::PAYLOAD_TOO_LARGE, "Video is longer than 3 minutes"));
    }

    let output = dir.path().join(format!("out.{ext}"));
    let run = Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-y", "-i"])
        .arg(&input)
        .args(["-vn", "-map", "0:a:0?", "-map_metadata", "-1"])
        .args(codec)
        .arg(&output)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output();
    let res = timeout(FFMPEG_TIMEOUT, run)
        .await
        .map_err(|_| fail(StatusCode::GATEWAY_TIMEOUT, "Conversion timed out"))?
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    if !res.status.success() || !output.exists() {
        return Err(fail(StatusCode::UNPROCESSABLE_ENTITY, "No audio track found or conversion failed"));
    }
    let bytes = tokio::fs::read(&output).await.map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    drop(dir);
    Ok((
        [
            (header::CONTENT_TYPE, mime.to_string()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{stem}.{ext}\"")),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ],
        bytes,
    )
        .into_response())
}

#[tokio::main]
async fn main() {
    SLOTS.set(Semaphore::new(2)).ok();
    let app = Router::new()
        .route("/", get(|| async { Html(include_str!("index.html")) }))
        .route("/convert", post(convert))
        .layer(DefaultBodyLimit::max(MAX_UPLOAD));
    let addr = std::env::var("BIND").unwrap_or_else(|_| "127.0.0.1:8444".into());
    let l = tokio::net::TcpListener::bind(&addr).await.expect("bind");
    println!("listening on {addr}");
    axum::serve(l, app).await.unwrap();
}
