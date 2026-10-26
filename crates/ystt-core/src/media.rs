//! Media file downloading: single stream, range-first parallel, validation.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use http::header::{ACCEPT_RANGES, CONTENT_RANGE, ETAG, HeaderValue, IF_RANGE, LAST_MODIFIED};
use serde::{Deserialize, Serialize};
use tokio::fs::{self, File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt, BufWriter};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};
use ystt_net::{BrowserHeaders, HttpClient, HttpResponse, NetError, RequestKind, RequestOptions};

use crate::error::{Result, YsttError};
use crate::model::{MediaKind, is_https_url};
use crate::plan::{AdaptiveRangeController, ByteRange, RangePlanner};
use crate::progress::{MediaTarget, ProgressEmitter, ProgressTracker};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// One downloaded media file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadedFile {
    /// Absolute or caller-supplied path.
    pub path: PathBuf,
    /// MIME type.
    pub mime: String,
    /// Extension without a dot.
    pub ext: String,
    /// Size in bytes.
    pub size: u64,
    /// Source CDN URL.
    pub url: String,
    /// Media kind.
    pub kind: MediaKind,
}

/// Shared context for one media transfer.
#[derive(Clone)]
pub(crate) struct MediaContext<'a> {
    pub client: &'a HttpClient,
    pub referer: &'a str,
    pub cookie: Option<&'a str>,
    pub signal: Option<&'a CancellationToken>,
    pub max_bytes: u64,
    pub read_timeout: Duration,
    pub parallel_ranges: bool,
    pub max_connections: usize,
    pub first_chunk: u64,
    pub min_chunk: u64,
    pub range_threshold: u64,
    pub reuse_existing: bool,
    pub media_retries: u32,
    pub progress: Option<ProgressEmitter>,
    pub adaptive_ranges: bool,
    pub range_start_connections: usize,
}

/// Download one media resource into `path`.
pub(crate) async fn download_media(
    context: &MediaContext<'_>,
    url: &str,
    path: &Path,
    kind: MediaKind,
    target: MediaTarget,
) -> Result<DownloadedFile> {
    if !is_https_url(url) {
        return Err(YsttError::Parse(format!(
            "media URL is not absolute HTTPS: {url}"
        )));
    }

    if context.reuse_existing
        && let Ok(metadata) = fs::metadata(path).await
        && metadata.is_file()
        && metadata.len() > 0
    {
        return DownloadedFile::from_path(path, url, kind, metadata.len());
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }

    let tracker = context
        .progress
        .clone()
        .map(|emitter| ProgressTracker::new(emitter, target, None));

    let size = match kind {
        MediaKind::Video if context.parallel_ranges => {
            match download_adaptive(context, url, path, tracker.clone()).await {
                Ok(size) => size,
                Err(error) if should_fallback(&error) => {
                    warn!(?error, "range download failed; falling back to one stream");
                    download_single(context, url, path, target, tracker.clone()).await?
                }
                Err(error) => return Err(error),
            }
        }
        _ => download_single(context, url, path, target, tracker.clone()).await?,
    };

    if let Some(tracker) = &tracker {
        tracker.complete();
    }
    DownloadedFile::from_path(path, url, kind, size)
}

fn should_fallback(error: &YsttError) -> bool {
    matches!(
        error,
        YsttError::Range(_)
            | YsttError::Network(_)
            | YsttError::Parse(_)
            | YsttError::Unavailable(_)
    )
}

async fn download_single(
    context: &MediaContext<'_>,
    url: &str,
    path: &Path,
    target: MediaTarget,
    tracker: Option<std::sync::Arc<ProgressTracker>>,
) -> Result<u64> {
    let kind = target.kind;
    let headers = BrowserHeaders::build(
        request_kind(kind),
        Some(context.referer),
        context.cookie,
        None,
    )?;
    let request_started = Instant::now();
    let response = context
        .client
        .get(
            url,
            headers,
            media_options(context.signal, context.media_retries),
        )
        .await?;
    let headers_elapsed = request_started.elapsed();

    if !response.is_success() {
        return Err(media_status_error(&response));
    }

    let version = response.version();
    let body_started = Instant::now();
    let written = write_response_to_path(context, response, path, kind, tracker.as_deref()).await?;
    debug!(
        ?version,
        headers_ms = headers_elapsed.as_millis() as u64,
        body_ms = body_started.elapsed().as_millis() as u64,
        bytes = written,
        "single media transfer"
    );
    Ok(written)
}

async fn download_adaptive(
    context: &MediaContext<'_>,
    url: &str,
    path: &Path,
    tracker: Option<std::sync::Arc<ProgressTracker>>,
) -> Result<u64> {
    let headers = BrowserHeaders::build(
        RequestKind::Video,
        Some(context.referer),
        context.cookie,
        None,
    )?;
    let request_started = Instant::now();
    let response = context
        .client
        .get(
            url,
            headers,
            media_options(context.signal, context.media_retries),
        )
        .await?;
    let headers_elapsed = request_started.elapsed();

    let status = response.status_u16();
    if !(200..300).contains(&status) {
        return Err(media_status_error(&response));
    }
    let version = response.version();

    let total = response.content_length();
    let accepts_ranges = response
        .headers()
        .get(ACCEPT_RANGES)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("bytes"));
    let should_split = context.max_connections > 1
        && accepts_ranges
        && total.is_some_and(|total| {
            total > context.range_threshold
                && total > context.first_chunk
                && total <= context.max_bytes
        });

    if !should_split {
        let body_started = Instant::now();
        let written = write_response_to_path(
            context,
            response,
            path,
            MediaKind::Video,
            tracker.as_deref(),
        )
        .await?;
        debug!(
            ?version,
            headers_ms = headers_elapsed.as_millis() as u64,
            body_ms = body_started.elapsed().as_millis() as u64,
            bytes = written,
            "adaptive single media transfer"
        );
        return Ok(written);
    }

    let total = total.unwrap_or(0);
    let etag = header_string(&response, ETAG);
    let last_modified = header_string(&response, LAST_MODIFIED);
    let temp = temp_path(path);
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&temp)
        .await?;
    file.set_len(total).await?;
    if let Some(tracker) = &tracker {
        tracker.set_total(total);
    }

    let mut writer = BufWriter::with_capacity(2 * 1024 * 1024, file);
    let prefix = write_stream_prefix(
        response,
        &mut writer,
        context.read_timeout,
        context.signal,
        context.first_chunk,
        tracker.as_deref(),
    )
    .await;
    let prefix = match prefix {
        Ok(prefix) => prefix,
        Err(error) => {
            drop(writer);
            remove_quietly(&temp).await;
            return Err(error);
        }
    };
    drop(writer);

    if prefix < context.first_chunk {
        if prefix == total {
            if let Err(error) = validate_download(&temp, MediaKind::Video).await {
                remove_quietly(&temp).await;
                return Err(error);
            }
            if let Err(error) = fs::rename(&temp, path).await {
                remove_quietly(&temp).await;
                return Err(error.into());
            }
            return Ok(total);
        }
        remove_quietly(&temp).await;
        return Err(YsttError::Range(format!(
            "initial body ended after {prefix} of {total} bytes"
        )));
    }

    if let Err(error) = download_remaining(
        context,
        url,
        &temp,
        total,
        prefix,
        etag,
        last_modified,
        tracker,
    )
    .await
    {
        remove_quietly(&temp).await;
        return Err(error);
    }

    if let Err(error) = validate_download(&temp, MediaKind::Video).await {
        remove_quietly(&temp).await;
        return Err(error);
    }
    if let Err(error) = fs::rename(&temp, path).await {
        remove_quietly(&temp).await;
        return Err(error.into());
    }

    debug!(
        ?version,
        headers_ms = headers_elapsed.as_millis() as u64,
        prefix_bytes = prefix,
        total,
        "adaptive range media transfer"
    );
    Ok(total)
}

#[allow(clippy::too_many_arguments)]
async fn download_remaining(
    context: &MediaContext<'_>,
    url: &str,
    temp: &Path,
    total: u64,
    first_len: u64,
    etag: Option<String>,
    last_modified: Option<String>,
    tracker: Option<std::sync::Arc<ProgressTracker>>,
) -> Result<()> {
    let plan = RangePlanner::new(
        context.max_connections,
        context.first_chunk,
        context.min_chunk,
    )
    .plan_with_first(total, first_len);

    debug!(
        total,
        first_len,
        ranges = plan.ranges.len(),
        "downloading video with ranges"
    );

    if !plan.is_parallel() {
        return Ok(());
    }

    let mut pending = plan.rest_ranges().iter().copied();
    let mut tasks = JoinSet::new();
    let max_rest = context.max_connections.saturating_sub(1).max(1);
    let start = if context.adaptive_ranges {
        context.range_start_connections.min(max_rest).max(1)
    } else {
        max_rest
    };
    let mut controller = AdaptiveRangeController::new(max_rest, start);

    loop {
        while tasks.len() < controller.limit() {
            let Some(range) = pending.next() else {
                break;
            };
            spawn_range_task(
                &mut tasks,
                context.client.clone(),
                url.to_owned(),
                temp.to_path_buf(),
                range,
                etag.clone(),
                last_modified.clone(),
                context.referer.to_owned(),
                context.cookie.map(str::to_owned),
                context.read_timeout,
                context.signal.cloned(),
                total,
                tracker.clone(),
                context.media_retries,
            );
        }

        let Some(joined) = tasks.join_next().await else {
            break;
        };
        match joined {
            Ok((range, Ok(()), elapsed)) => {
                if context.adaptive_ranges {
                    let _ = controller.observe(range.len(), elapsed.as_secs_f64());
                    debug!(
                        limit = controller.limit(),
                        range_start = range.start,
                        range_end = range.end,
                        elapsed_ms = elapsed.as_millis() as u64,
                        "range completed"
                    );
                }
            }
            Ok((_range, Err(error), _)) => {
                tasks.abort_all();
                return Err(error);
            }
            Err(error) => {
                tasks.abort_all();
                return Err(YsttError::Internal(error.to_string()));
            }
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn spawn_range_task(
    tasks: &mut JoinSet<(ByteRange, Result<()>, Duration)>,
    client: HttpClient,
    url: String,
    temp: PathBuf,
    range: ByteRange,
    etag: Option<String>,
    last_modified: Option<String>,
    referer: String,
    cookie: Option<String>,
    read_timeout: Duration,
    signal: Option<CancellationToken>,
    expected_total: u64,
    tracker: Option<std::sync::Arc<ProgressTracker>>,
    media_retries: u32,
) {
    tasks.spawn(async move {
        let started = Instant::now();
        let result = download_range(
            client,
            url,
            temp,
            range,
            etag,
            last_modified,
            referer,
            cookie,
            read_timeout,
            signal,
            expected_total,
            tracker,
            media_retries,
        )
        .await;
        (range, result, started.elapsed())
    });
}

#[allow(clippy::too_many_arguments)]
async fn download_range(
    client: HttpClient,
    url: String,
    temp: PathBuf,
    range: ByteRange,
    etag: Option<String>,
    last_modified: Option<String>,
    referer: String,
    cookie: Option<String>,
    read_timeout: Duration,
    signal: Option<CancellationToken>,
    expected_total: u64,
    tracker: Option<std::sync::Arc<ProgressTracker>>,
    media_retries: u32,
) -> Result<()> {
    let mut headers = BrowserHeaders::build(
        RequestKind::Video,
        Some(&referer),
        cookie.as_deref(),
        Some((range.start, range.end)),
    )?;

    if let Some(value) = etag {
        let value = HeaderValue::from_str(&value)
            .map_err(|error| YsttError::Range(format!("invalid ETag: {error}")))?;
        headers.insert(IF_RANGE, value);
    } else if let Some(value) = last_modified {
        let value = HeaderValue::from_str(&value)
            .map_err(|error| YsttError::Range(format!("invalid Last-Modified: {error}")))?;
        headers.insert(IF_RANGE, value);
    }

    let mut options = RequestOptions::media().with_retries(media_retries);
    if let Some(signal) = &signal {
        options = options.with_signal(signal.clone());
    }

    let response = client.get(&url, headers, options).await?;
    if response.status_u16() != 206 {
        return Err(YsttError::Range(format!(
            "range {}..{} returned {}",
            range.start,
            range.end,
            response.status_u16()
        )));
    }

    let content_range = response
        .headers()
        .get(CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let Some((start, end, total)) = content_range.as_deref().and_then(parse_content_range) else {
        return Err(YsttError::Range(
            "range response has no Content-Range".into(),
        ));
    };
    if start != range.start || end != range.end || total != Some(expected_total) {
        return Err(YsttError::Range(format!(
            "unexpected Content-Range for {}..{}: {start}-{end}/{total:?}",
            range.start, range.end
        )));
    }

    let mut file = OpenOptions::new().write(true).open(&temp).await?;
    file.seek(std::io::SeekFrom::Start(range.start)).await?;
    let mut writer = BufWriter::with_capacity(1024 * 1024, file);
    let written = write_stream(
        response,
        &mut writer,
        read_timeout,
        signal.as_ref(),
        range.len(),
        tracker.as_deref(),
    )
    .await?;
    drop(writer);

    if written != range.len() {
        return Err(YsttError::Range(format!(
            "range {}..{} wrote {written} of {} bytes",
            range.start,
            range.end,
            range.len()
        )));
    }

    Ok(())
}

async fn write_response_to_path(
    context: &MediaContext<'_>,
    response: HttpResponse,
    path: &Path,
    kind: MediaKind,
    tracker: Option<&ProgressTracker>,
) -> Result<u64> {
    if let Some(length) = response.content_length()
        && length > context.max_bytes
    {
        return Err(YsttError::FileTooLarge {
            size: length,
            limit: context.max_bytes,
        });
    }
    if let Some(tracker) = tracker
        && let Some(length) = response.content_length()
    {
        tracker.set_total(length);
    }

    let temp = temp_path(path);
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&temp)
        .await?;
    let mut writer = BufWriter::with_capacity(2 * 1024 * 1024, file);

    let written = match write_stream(
        response,
        &mut writer,
        context.read_timeout,
        context.signal,
        context.max_bytes,
        tracker,
    )
    .await
    {
        Ok(written) => written,
        Err(error) => {
            drop(writer);
            remove_quietly(&temp).await;
            return Err(error);
        }
    };
    drop(writer);

    if written == 0 {
        remove_quietly(&temp).await;
        return Err(YsttError::Parse("empty media body".into()));
    }

    if let Err(error) = validate_download(&temp, kind).await {
        remove_quietly(&temp).await;
        return Err(error);
    }

    if let Err(error) = fs::rename(&temp, path).await {
        remove_quietly(&temp).await;
        return Err(error.into());
    }

    Ok(written)
}

async fn write_stream_prefix<W>(
    response: HttpResponse,
    writer: &mut W,
    read_timeout: Duration,
    signal: Option<&CancellationToken>,
    limit: u64,
    tracker: Option<&ProgressTracker>,
) -> Result<u64>
where
    W: AsyncWrite + Unpin,
{
    let mut stream = Box::pin(response.into_stream());
    let mut total = 0u64;

    while total < limit {
        let next = tokio::time::timeout(read_timeout, stream.next());
        let item = match signal {
            Some(signal) => {
                tokio::select! {
                    _ = signal.cancelled() => return Err(YsttError::Cancelled),
                    item = next => item,
                }
            }
            None => next.await,
        };

        match item {
            Err(_) => return Err(YsttError::Network(NetError::Timeout)),
            Ok(None) => break,
            Ok(Some(Err(error))) => return Err(YsttError::Network(error)),
            Ok(Some(Ok(chunk))) => {
                let remaining = limit - total;
                let take = remaining.min(chunk.len() as u64) as usize;
                writer.write_all(&chunk[..take]).await?;
                total += take as u64;
                if let Some(tracker) = tracker {
                    tracker.add(take as u64);
                }
            }
        }
    }

    writer.flush().await?;
    Ok(total)
}

async fn write_stream<W>(
    response: HttpResponse,
    writer: &mut W,
    read_timeout: Duration,
    signal: Option<&CancellationToken>,
    limit: u64,
    tracker: Option<&ProgressTracker>,
) -> Result<u64>
where
    W: AsyncWrite + Unpin,
{
    let mut stream = Box::pin(response.into_stream());
    let mut total = 0u64;

    loop {
        let next = tokio::time::timeout(read_timeout, stream.next());
        let item = match signal {
            Some(signal) => {
                tokio::select! {
                    _ = signal.cancelled() => return Err(YsttError::Cancelled),
                    item = next => item,
                }
            }
            None => next.await,
        };

        match item {
            Err(_) => return Err(YsttError::Network(NetError::Timeout)),
            Ok(None) => break,
            Ok(Some(Err(error))) => return Err(YsttError::Network(error)),
            Ok(Some(Ok(chunk))) => {
                total = total.saturating_add(chunk.len() as u64);
                if total > limit {
                    return Err(YsttError::FileTooLarge { size: total, limit });
                }
                writer.write_all(&chunk).await?;
                if let Some(tracker) = tracker {
                    tracker.add(chunk.len() as u64);
                }
            }
        }
    }

    writer.flush().await?;
    Ok(total)
}

fn media_options(signal: Option<&CancellationToken>, retries: u32) -> RequestOptions {
    let options = RequestOptions::media().with_retries(retries);
    match signal {
        Some(signal) => options.with_signal(signal.clone()),
        None => options,
    }
}

fn media_status_error(response: &HttpResponse) -> YsttError {
    let status = response.status_u16();
    let url = response.url().to_owned();
    match status {
        404 | 410 => YsttError::Unavailable(format!("media HTTP {status}")),
        _ => YsttError::Network(NetError::Status { status, url }),
    }
}

fn request_kind(kind: MediaKind) -> RequestKind {
    match kind {
        MediaKind::Video => RequestKind::Video,
        MediaKind::Photo => RequestKind::Image,
        MediaKind::Audio => RequestKind::Audio,
    }
}

fn header_string(response: &HttpResponse, name: http::header::HeaderName) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

fn temp_path(path: &Path) -> PathBuf {
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos())
        .unwrap_or(0);
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "media".into());
    path.with_file_name(format!(
        ".{name}.{}.{}.{}.part",
        std::process::id(),
        nanos,
        counter
    ))
}

async fn remove_quietly(path: &Path) {
    let _ = fs::remove_file(path).await;
}

async fn validate_download(path: &Path, kind: MediaKind) -> Result<()> {
    let mut file = File::open(path).await?;
    let mut head = [0u8; 16];
    let read = file.read(&mut head).await?;
    let head = &head[..read];

    if head.is_empty() {
        return Err(YsttError::Parse("empty media file".into()));
    }
    if head[0] == b'<'
        || head.starts_with(b"<!DOCTYPE")
        || head.starts_with(b"<html")
        || head.starts_with(b"<HTML")
    {
        return Err(YsttError::Parse(
            "server returned an HTML page instead of media".into(),
        ));
    }

    let valid = match kind {
        MediaKind::Video => is_mp4(head) || is_webm(head) || is_riff_avi(head),
        MediaKind::Photo => is_jpeg(head) || is_png(head) || is_webp(head) || is_gif(head),
        MediaKind::Audio => is_mp3(head) || is_mp4(head) || is_ogg(head) || is_wav(head),
    };

    if !valid {
        return Err(YsttError::Parse(format!(
            "media signature mismatch for {}",
            kind.as_str()
        )));
    }

    Ok(())
}

fn is_mp4(head: &[u8]) -> bool {
    head.len() >= 8 && &head[4..8] == b"ftyp"
}

fn is_webm(head: &[u8]) -> bool {
    head.starts_with(&[0x1A, 0x45, 0xDF, 0xA3])
}

fn is_riff_avi(head: &[u8]) -> bool {
    head.len() >= 12 && head.starts_with(b"RIFF") && &head[8..12] == b"AVI "
}

fn is_jpeg(head: &[u8]) -> bool {
    head.starts_with(&[0xFF, 0xD8, 0xFF])
}

fn is_png(head: &[u8]) -> bool {
    head.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A])
}

fn is_webp(head: &[u8]) -> bool {
    head.len() >= 12 && head.starts_with(b"RIFF") && &head[8..12] == b"WEBP"
}

fn is_gif(head: &[u8]) -> bool {
    head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a")
}

fn is_mp3(head: &[u8]) -> bool {
    head.starts_with(b"ID3") || (head.len() >= 2 && head[0] == 0xFF && (head[1] & 0xE0) == 0xE0)
}

fn is_ogg(head: &[u8]) -> bool {
    head.starts_with(b"OggS")
}

fn is_wav(head: &[u8]) -> bool {
    head.len() >= 12 && head.starts_with(b"RIFF") && &head[8..12] == b"WAVE"
}

/// Parse `Content-Range: bytes 0-1023/2048`.
pub(crate) fn parse_content_range(value: &str) -> Option<(u64, u64, Option<u64>)> {
    let rest = value.trim().strip_prefix("bytes ")?;
    let (range, total) = rest.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start = start.trim().parse().ok()?;
    let end = end.trim().parse().ok()?;
    let total = if total.trim() == "*" {
        None
    } else {
        Some(total.trim().parse().ok()?)
    };
    Some((start, end, total))
}

/// Lowercase extension from a URL path, if it is safe.
pub(crate) fn ext_from_url(url: &str, fallback: &str) -> String {
    let Ok(url) = url::Url::parse(url) else {
        return fallback.to_owned();
    };
    let Some(segment) = url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
    else {
        return fallback.to_owned();
    };
    let Some((_, ext)) = segment.rsplit_once('.') else {
        return fallback.to_owned();
    };
    let ext = ext.to_ascii_lowercase();
    if !ext.is_empty()
        && ext
            .chars()
            .all(|character| character.is_ascii_alphanumeric())
    {
        ext
    } else {
        fallback.to_owned()
    }
}

/// Extension for a video URL.
pub(crate) fn video_ext(url: &str) -> String {
    match ext_from_url(url, "mp4").as_str() {
        "mp4" | "webm" | "mov" | "m4v" => ext_from_url(url, "mp4"),
        _ => "mp4".into(),
    }
}

/// Extension for a photo URL.
pub(crate) fn photo_ext(url: &str) -> String {
    match ext_from_url(url, "jpg").as_str() {
        "png" => "png".into(),
        "webp" => "webp".into(),
        "gif" => "gif".into(),
        "jpeg" | "jpg" => "jpg".into(),
        _ => "jpg".into(),
    }
}

/// Extension for an audio URL.
pub(crate) fn audio_ext(url: &str) -> String {
    match ext_from_url(url, "mp3").as_str() {
        "mp3" | "m4a" | "aac" | "ogg" | "opus" => ext_from_url(url, "mp3"),
        _ => "mp3".into(),
    }
}

/// MIME type for an extension and media kind.
pub(crate) fn mime_for_ext(ext: &str, kind: MediaKind) -> String {
    let mime = match ext {
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "mp3" => "audio/mpeg",
        "m4a" | "aac" => "audio/mp4",
        "ogg" | "opus" => "audio/ogg",
        "wav" => "audio/wav",
        _ => match kind {
            MediaKind::Video => "video/mp4",
            MediaKind::Photo => "image/jpeg",
            MediaKind::Audio => "audio/mpeg",
        },
    };
    mime.to_owned()
}

/// Sanitize a generated filename for all supported operating systems.
pub fn sanitize_filename(value: &str) -> String {
    let mut sanitized = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_control()
            || matches!(
                character,
                '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
            )
        {
            sanitized.push('_');
        } else {
            sanitized.push(character);
        }
    }

    let sanitized = sanitized.trim().trim_matches('.').trim();
    let mut sanitized = if sanitized.is_empty() {
        "tiktok".to_owned()
    } else {
        sanitized.chars().take(180).collect()
    };

    let base = sanitized
        .split('.')
        .next()
        .unwrap_or("tiktok")
        .to_ascii_uppercase();
    if matches!(
        base.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    ) {
        sanitized.insert(0, '_');
    }

    sanitized
}

/// Base filename shared by all files of one post.
pub(crate) fn base_filename(author_id: &str, post_id: &str) -> String {
    sanitize_filename(&format!("tiktok_{author_id}_{post_id}"))
}

impl DownloadedFile {
    fn from_path(path: &Path, url: &str, kind: MediaKind, size: u64) -> Result<Self> {
        let ext = path
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("bin")
            .to_ascii_lowercase();
        let mime = mime_for_ext(&ext, kind);
        Ok(Self {
            path: path.to_path_buf(),
            mime,
            ext,
            size,
            url: url.to_owned(),
            kind,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_range_is_parsed() {
        assert_eq!(
            parse_content_range("bytes 0-1023/2048"),
            Some((0, 1023, Some(2048)))
        );
        assert_eq!(
            parse_content_range("bytes 1024-2047/*"),
            Some((1024, 2047, None))
        );
        assert_eq!(parse_content_range("garbage"), None);
    }

    #[test]
    fn extensions_are_normalized() {
        assert_eq!(video_ext("https://cdn/video.mp4?x=1"), "mp4");
        assert_eq!(video_ext("https://cdn/video.unknown?x=1"), "mp4");
        assert_eq!(photo_ext("https://cdn/p.jpeg?x=1"), "jpg");
        assert_eq!(photo_ext("https://cdn/p.webp"), "webp");
        assert_eq!(audio_ext("https://cdn/a.m4a?x=1"), "m4a");
    }

    #[test]
    fn filenames_are_safe() {
        assert_eq!(sanitize_filename("a/b:c*?"), "a_b_c__");
        assert_eq!(sanitize_filename(".."), "tiktok");
        assert_eq!(sanitize_filename("CON"), "_CON");
        assert!(sanitize_filename(&"x".repeat(400)).len() <= 180);
    }

    #[test]
    fn signatures_are_checked() {
        assert!(is_mp4(b"\0\0\0\x20ftypisom"));
        assert!(is_webm(&[0x1A, 0x45, 0xDF, 0xA3]));
        assert!(is_jpeg(&[0xFF, 0xD8, 0xFF, 0xE0]));
        assert!(is_png(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]));
        assert!(is_webp(b"RIFF\0\0\0\0WEBP"));
        assert!(!is_jpeg(b"<html>"));
    }

    #[tokio::test]
    async fn html_is_rejected_as_media() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.mp4");
        tokio::fs::write(&path, b"<html>blocked</html>")
            .await
            .unwrap();
        assert!(matches!(
            validate_download(&path, MediaKind::Video).await,
            Err(YsttError::Parse(_))
        ));
    }
}
