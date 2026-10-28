//! Single-stream, resumable and adaptive range transfers.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::{Bytes, BytesMut};
use futures_util::{StreamExt, stream};
use http::header::{
    ACCEPT_RANGES, CONTENT_RANGE, ETAG, HeaderValue, IF_RANGE, LAST_MODIFIED, RANGE,
};
use serde::{Deserialize, Serialize};
use tokio::fs::{self, OpenOptions};
use tokio::io::{AsyncSeekExt, AsyncWrite, AsyncWriteExt, BufWriter};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::debug;
use ystt_net::{BrowserHeaders, HttpClient, HttpResponse, NetError, RequestKind, RequestOptions};

use crate::cache_util::temp_sibling;
use crate::error::{Result, YsttError};
use crate::model::MediaKind;
use crate::plan::{AdaptiveRangeController, ByteRange, RangePlanner};
use crate::progress::{MediaTarget, ProgressTracker};

use super::MediaContext;
use super::validation::{validate_bytes, validate_download};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RangeCheckpoint {
    total: u64,
    etag: Option<String>,
    last_modified: Option<String>,
    completed: Vec<ByteRange>,
    created_unix: u64,
}

struct RangeResumeState<'a> {
    path: &'a Path,
    total: u64,
    etag: Option<String>,
    last_modified: Option<String>,
    completed: Vec<ByteRange>,
}

impl RangeResumeState<'_> {
    async fn save(&self) -> Result<()> {
        let checkpoint = RangeCheckpoint {
            total: self.total,
            etag: self.etag.clone(),
            last_modified: self.last_modified.clone(),
            completed: self.completed.clone(),
            created_unix: unix_now(),
        };
        let bytes = serde_json::to_vec(&checkpoint)?;
        let temp = temp_sibling(self.path);
        fs::write(&temp, bytes).await?;
        fs::rename(&temp, self.path).await?;
        Ok(())
    }
}

pub(crate) fn should_fallback(error: &YsttError) -> bool {
    matches!(
        error,
        YsttError::Range(_)
            | YsttError::Network(_)
            | YsttError::Parse(_)
            | YsttError::Unavailable(_)
    )
}

pub(crate) async fn download_resume(
    context: &MediaContext<'_>,
    url: &str,
    path: &Path,
    target: MediaTarget,
    tracker: Option<std::sync::Arc<ProgressTracker>>,
) -> Result<u64> {
    let part = resume_part_path(path);
    let offset = fs::metadata(&part).await.map(|m| m.len()).unwrap_or(0);
    let mut headers = BrowserHeaders::build(
        request_kind(target.kind),
        Some(context.referer),
        context.cookie,
        None,
    )?;
    if offset > 0 {
        let value = HeaderValue::from_str(&format!("bytes={offset}-"))
            .map_err(|error| YsttError::Range(format!("invalid range header: {error}")))?;
        headers.insert(RANGE, value);
    }
    let request_started = Instant::now();
    let response = context
        .client
        .get(
            url,
            headers,
            media_options(context.signal, context.media_retries),
        )
        .await?;
    let status = response.status_u16();
    if offset > 0 && status == 206 {
        let content_range = response
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .and_then(parse_content_range);
        let Some((start, _end, Some(total))) = content_range else {
            remove_quietly(&part).await;
            return Err(YsttError::Range(
                "resume response has no usable Content-Range".into(),
            ));
        };
        if start != offset || total > context.max_bytes {
            remove_quietly(&part).await;
            return Err(YsttError::Range(format!(
                "unexpected resume range start={start} offset={offset} total={total}"
            )));
        }
        if let Some(tracker) = &tracker {
            tracker.set_total(total);
            tracker.add(offset);
        }
        let file = OpenOptions::new().append(true).open(&part).await?;
        let mut writer = BufWriter::with_capacity(2 * 1024 * 1024, file);
        let written = match write_stream(
            response,
            &mut writer,
            context.read_timeout,
            context.signal,
            total,
            tracker.as_deref(),
        )
        .await
        {
            Ok(written) => written,
            Err(error) => {
                drop(writer);
                return Err(error);
            }
        };
        drop(writer);
        if offset.saturating_add(written) != total {
            return Err(YsttError::Range(format!(
                "resume wrote {} of {} bytes",
                offset.saturating_add(written),
                total
            )));
        }
        if let Err(error) = validate_download(&part, target.kind).await {
            remove_quietly(&part).await;
            return Err(error);
        }
        if let Err(error) = fs::rename(&part, path).await {
            remove_quietly(&part).await;
            return Err(error.into());
        }
        debug!(
            bytes = total,
            offset,
            headers_ms = request_started.elapsed().as_millis() as u64,
            "resumed media transfer"
        );
        return Ok(total);
    }
    if !response.is_success() {
        return Err(media_status_error(&response));
    }
    // A server that ignores Range returns 200; restart cleanly.
    write_response_to_temp(
        context,
        response,
        &part,
        path,
        target.kind,
        tracker.as_deref(),
    )
    .await
}

pub(crate) async fn download_single(
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

/// Download one media resource into memory, without creating any file.
///
/// Large responses are split into parallel HTTP ranges when the CDN advertises
/// range support. The first response body is used for the prefix, so the
/// common case still costs exactly one request.
pub(crate) async fn download_media_bytes(
    context: &MediaContext<'_>,
    url: &str,
    target: MediaTarget,
    tracker: Option<std::sync::Arc<ProgressTracker>>,
) -> Result<Bytes> {
    let headers = BrowserHeaders::build(
        request_kind(target.kind),
        Some(context.referer),
        context.cookie,
        None,
    )?;
    let response = context
        .client
        .get(
            url,
            headers,
            media_options(context.signal, context.media_retries),
        )
        .await?;
    if !response.is_success() {
        return Err(media_status_error(&response));
    }

    let total = response.content_length();
    if let Some(total) = total {
        if total > context.max_bytes {
            return Err(YsttError::FileTooLarge {
                size: total,
                limit: context.max_bytes,
            });
        }
        if let Some(tracker) = &tracker {
            tracker.set_total(total);
        }
    }

    let accepts_ranges = response
        .headers()
        .get(ACCEPT_RANGES)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("bytes"));
    let should_split = context.parallel_ranges
        && context.max_connections > 1
        && accepts_ranges
        && total.is_some_and(|total| {
            total > context.range_threshold
                && total > context.first_chunk
                && total <= context.max_bytes
        });

    if !should_split {
        return collect_bytes_response(context, response, target.kind, tracker).await;
    }

    let total = total.ok_or_else(|| YsttError::Range("range response has no length".into()))?;
    let etag = header_string(&response, ETAG);
    let last_modified = header_string(&response, LAST_MODIFIED);
    let plan = RangePlanner::new(
        context.max_connections,
        context.first_chunk,
        context.min_chunk,
    )
    .plan(total);
    let first = plan
        .first_range()
        .ok_or_else(|| YsttError::Range("empty range plan".into()))?;
    let first_len = first.len();
    let prefix_started = Instant::now();
    let prefix =
        read_stream_prefix(response, first_len, context.read_timeout, context.signal).await;
    let prefix = match prefix {
        Ok(prefix) => prefix,
        Err(error) => {
            return if should_fallback(&error) {
                debug!(?error, "in-memory prefix failed; retrying single stream");
                download_media_bytes_single(context, url, target, tracker).await
            } else {
                Err(error)
            };
        }
    };
    if let Some(tracker) = &tracker {
        tracker.add(prefix.len() as u64);
    }
    if prefix.len() as u64 != first_len {
        let error = YsttError::Range(format!(
            "in-memory prefix ended after {} of {first_len} bytes",
            prefix.len()
        ));
        debug!(
            ?error,
            "in-memory prefix incomplete; retrying single stream"
        );
        return download_media_bytes_single(context, url, target, tracker.clone()).await;
    }

    let rest = plan.rest_ranges();
    let mut parts: Vec<Bytes> = Vec::with_capacity(rest.len());
    if !rest.is_empty() {
        let concurrency = context.max_connections.saturating_sub(1).max(1);
        let mut downloads = stream::iter(rest.iter().copied().enumerate().map(|(index, range)| {
            let etag = etag.clone();
            let last_modified = last_modified.clone();
            let tracker = tracker.clone();
            async move {
                let bytes =
                    download_range_bytes(context, url, range, etag, last_modified, tracker).await?;
                Ok::<_, YsttError>((index, bytes))
            }
        }))
        .buffer_unordered(concurrency);
        let mut ordered: Vec<Option<Bytes>> = vec![None; rest.len()];
        while let Some(result) = downloads.next().await {
            let (index, bytes) = match result {
                Ok(value) => value,
                Err(error) if should_fallback(&error) => {
                    debug!(?error, "in-memory range failed; retrying single stream");
                    return download_media_bytes_single(context, url, target, tracker.clone())
                        .await;
                }
                Err(error) => return Err(error),
            };
            ordered[index] = Some(bytes);
        }
        for part in ordered {
            parts.push(part.ok_or_else(|| YsttError::Range("missing range result".into()))?);
        }
    }

    let mut result = BytesMut::with_capacity(total as usize);
    result.extend_from_slice(&prefix);
    for part in parts {
        result.extend_from_slice(&part);
    }
    if result.len() as u64 != total {
        return Err(YsttError::Range(format!(
            "in-memory range assembly wrote {} of {total} bytes",
            result.len()
        )));
    }
    let bytes = result.freeze();
    validate_bytes(&bytes, target.kind)?;
    if let Some(tracker) = &tracker {
        tracker.complete();
    }
    debug!(
        total,
        prefix_bytes = first_len,
        ranges = plan.ranges.len(),
        prefix_ms = prefix_started.elapsed().as_millis() as u64,
        "in-memory range transfer"
    );
    Ok(bytes)
}

async fn download_media_bytes_single(
    context: &MediaContext<'_>,
    url: &str,
    target: MediaTarget,
    tracker: Option<std::sync::Arc<ProgressTracker>>,
) -> Result<Bytes> {
    let headers = BrowserHeaders::build(
        request_kind(target.kind),
        Some(context.referer),
        context.cookie,
        None,
    )?;
    let response = context
        .client
        .get(
            url,
            headers,
            media_options(context.signal, context.media_retries),
        )
        .await?;
    if !response.is_success() {
        return Err(media_status_error(&response));
    }
    if let Some(total) = response.content_length() {
        if total > context.max_bytes {
            return Err(YsttError::FileTooLarge {
                size: total,
                limit: context.max_bytes,
            });
        }
        if let Some(tracker) = &tracker {
            tracker.set_total(total);
        }
    }
    collect_bytes_response(context, response, target.kind, tracker).await
}

async fn collect_bytes_response(
    context: &MediaContext<'_>,
    response: HttpResponse,
    kind: MediaKind,
    tracker: Option<std::sync::Arc<ProgressTracker>>,
) -> Result<Bytes> {
    let version = response.version();
    let body_started = Instant::now();
    let bytes = response
        .bytes(context.max_bytes, context.read_timeout, context.signal)
        .await?;
    validate_bytes(&bytes, kind)?;
    if let Some(tracker) = &tracker {
        tracker.add(bytes.len() as u64);
        tracker.complete();
    }
    debug!(
        ?version,
        body_ms = body_started.elapsed().as_millis() as u64,
        bytes = bytes.len(),
        "in-memory media transfer"
    );
    Ok(bytes)
}

async fn read_stream_prefix(
    response: HttpResponse,
    limit: u64,
    read_timeout: Duration,
    signal: Option<&CancellationToken>,
) -> Result<Bytes> {
    let mut stream = response.into_stream();
    let mut buffer = BytesMut::with_capacity(limit.min(64 * 1024 * 1024) as usize);
    while (buffer.len() as u64) < limit {
        let next = tokio::time::timeout(read_timeout, stream.next());
        let item = match signal {
            Some(signal) => {
                tokio::select! {
                    _ = signal.cancelled() => return Err(YsttError::Cancelled),
                    result = next => result,
                }
            }
            None => next.await,
        };
        match item {
            Err(_) => return Err(YsttError::Network(NetError::Timeout)),
            Ok(None) => break,
            Ok(Some(Err(error))) => return Err(error.into()),
            Ok(Some(Ok(chunk))) => {
                let remaining = limit - buffer.len() as u64;
                let take = (chunk.len() as u64).min(remaining) as usize;
                buffer.extend_from_slice(&chunk[..take]);
                if take < chunk.len() {
                    break;
                }
            }
        }
    }
    Ok(buffer.freeze())
}

async fn download_range_bytes(
    context: &MediaContext<'_>,
    url: &str,
    range: ByteRange,
    etag: Option<String>,
    last_modified: Option<String>,
    tracker: Option<std::sync::Arc<ProgressTracker>>,
) -> Result<Bytes> {
    let mut headers = BrowserHeaders::build(
        RequestKind::Video,
        Some(context.referer),
        context.cookie,
        None,
    )?;
    headers.insert(
        RANGE,
        HeaderValue::from_str(&format!("bytes={}-{}", range.start, range.end))
            .map_err(|error| YsttError::Range(format!("invalid range header: {error}")))?,
    );
    if let Some(value) = etag.or(last_modified)
        && let Ok(value) = HeaderValue::from_str(&value)
    {
        headers.insert(IF_RANGE, value);
    }
    let response = context
        .client
        .get(
            url,
            headers,
            media_options(context.signal, context.media_retries),
        )
        .await?;
    if response.status_u16() != 206 {
        return Err(media_status_error(&response));
    }
    let content_range = response
        .headers()
        .get(CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_content_range)
        .ok_or_else(|| YsttError::Range("range response has no Content-Range".into()))?;
    if content_range.0 != range.start
        || content_range.1 != range.end
        || content_range
            .2
            .is_some_and(|total| total > context.max_bytes)
    {
        return Err(YsttError::Range(format!(
            "unexpected range {}-{} got {content_range:?}",
            range.start, range.end
        )));
    }
    let bytes = response
        .bytes(range.len().max(1), context.read_timeout, context.signal)
        .await?;
    if bytes.len() as u64 != range.len() {
        return Err(YsttError::Range(format!(
            "range {}-{} returned {} bytes",
            range.start,
            range.end,
            bytes.len()
        )));
    }
    if let Some(tracker) = &tracker {
        tracker.add(bytes.len() as u64);
    }
    Ok(bytes)
}

pub(crate) async fn download_adaptive(
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
    let resume = context.resume;
    let temp = if resume {
        adaptive_part_path(path)
    } else {
        temp_path(path)
    };
    let checkpoint_path = adaptive_checkpoint_path(path);
    let mut completed = Vec::new();
    let mut reused = false;
    if resume
        && let Some(checkpoint) = load_checkpoint(&checkpoint_path).await
        && checkpoint.total == total
        && checkpoint.etag == etag
        && checkpoint.last_modified == last_modified
        && fs::metadata(&temp)
            .await
            .map(|metadata| metadata.len() == total)
            .unwrap_or(false)
    {
        completed = checkpoint.completed;
        reused = true;
    } else {
        remove_quietly(&temp).await;
        remove_quietly(&checkpoint_path).await;
    }
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(!reused)
        .open(&temp)
        .await?;
    file.set_len(total).await?;
    if let Some(tracker) = &tracker {
        tracker.set_total(total);
    }

    let prefix_range = ByteRange {
        start: 0,
        end: context
            .first_chunk
            .saturating_sub(1)
            .min(total.saturating_sub(1)),
    };
    let prefix_done = covers(&completed, prefix_range);
    let mut state = RangeResumeState {
        path: &checkpoint_path,
        total,
        etag,
        last_modified,
        completed,
    };
    let prefix = if prefix_done {
        context.first_chunk.min(total)
    } else {
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
        drop(writer);
        match prefix {
            Ok(prefix) => {
                if prefix > 0 {
                    state.completed.push(ByteRange {
                        start: 0,
                        end: prefix - 1,
                    });
                    if resume {
                        state.save().await?;
                    }
                }
                prefix
            }
            Err(error) => {
                if resume {
                    let _ = state.save().await;
                } else {
                    remove_quietly(&temp).await;
                }
                return Err(error);
            }
        }
    };

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
            if resume {
                remove_quietly(&checkpoint_path).await;
            }
            return Ok(total);
        }
        if !resume {
            remove_quietly(&temp).await;
        }
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
        state.etag.clone(),
        state.last_modified.clone(),
        tracker,
        if resume { Some(&mut state) } else { None },
    )
    .await
    {
        if resume {
            let _ = state.save().await;
        } else {
            remove_quietly(&temp).await;
        }
        return Err(error);
    }

    if let Err(error) = validate_download(&temp, MediaKind::Video).await {
        if resume {
            let _ = state.save().await;
        } else {
            remove_quietly(&temp).await;
        }
        return Err(error);
    }
    if let Err(error) = fs::rename(&temp, path).await {
        if resume {
            let _ = state.save().await;
        } else {
            remove_quietly(&temp).await;
        }
        return Err(error.into());
    }
    if resume {
        remove_quietly(&checkpoint_path).await;
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
    mut resume: Option<&mut RangeResumeState<'_>>,
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

    let completed_snapshot = resume
        .as_ref()
        .map(|state| state.completed.clone())
        .unwrap_or_default();
    let mut pending = plan
        .rest_ranges()
        .iter()
        .copied()
        .filter(|range| !covers(&completed_snapshot, *range));
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
                if let Some(state) = resume.as_mut() {
                    state.completed.push(range);
                    state.save().await?;
                }
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
    let temp = temp_path(path);
    write_response_to_temp(context, response, &temp, path, kind, tracker).await
}

async fn write_response_to_temp(
    context: &MediaContext<'_>,
    response: HttpResponse,
    temp: &Path,
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

    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(temp)
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
            remove_quietly(temp).await;
            return Err(error);
        }
    };
    drop(writer);

    if written == 0 {
        remove_quietly(temp).await;
        return Err(YsttError::Parse("empty media body".into()));
    }

    if let Err(error) = validate_download(temp, kind).await {
        remove_quietly(temp).await;
        return Err(error);
    }

    if let Err(error) = fs::rename(temp, path).await {
        remove_quietly(temp).await;
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

fn adaptive_part_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "media".into());
    path.with_file_name(format!(".{name}.ranges.part"))
}

fn adaptive_checkpoint_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "media".into());
    path.with_file_name(format!(".{name}.ranges.json"))
}

fn covers(completed: &[ByteRange], range: ByteRange) -> bool {
    completed
        .iter()
        .any(|existing| existing.start <= range.start && existing.end >= range.end)
}

async fn load_checkpoint(path: &Path) -> Option<RangeCheckpoint> {
    let bytes = fs::read(path).await.ok()?;
    let checkpoint: RangeCheckpoint = serde_json::from_slice(&bytes).ok()?;
    let age = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs()
        .saturating_sub(checkpoint.created_unix);
    (age < 24 * 60 * 60).then_some(checkpoint)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

pub(crate) fn resume_part_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "media".into());
    path.with_file_name(format!(".{name}.part"))
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
