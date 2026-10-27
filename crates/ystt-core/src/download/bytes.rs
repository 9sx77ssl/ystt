//! In-memory video downloads for bots and other direct consumers.

use std::time::Instant;

use crate::config::DownloadOptions;
use crate::error::{Result, YsttError};
use crate::media::{MediaContext, download_media_bytes, mime_for_ext, video_ext};
use crate::model::MediaKind;
use crate::progress::{MediaTarget, ProgressEmitter, ProgressStage, ProgressTracker, next_job_id};

use super::options::ResolvedOptions;
use super::{InMemoryVideo, Ystt, millis};

impl Ystt {
    /// Download one video directly into memory with default options.
    ///
    /// Nothing is written to disk. The metadata/signed-URL cache still works as
    /// configured. Posts without a video return [`YsttError::NoVideo`].
    pub async fn download_video_bytes(&self, url: &str) -> Result<InMemoryVideo> {
        self.download_video_bytes_with(url, DownloadOptions::default())
            .await
    }

    /// Download one video directly into memory with per-call overrides.
    pub async fn download_video_bytes_with(
        &self,
        url: &str,
        options: DownloadOptions,
    ) -> Result<InMemoryVideo> {
        let job_id = next_job_id();
        let emitter = ProgressEmitter::single(options.progress.clone(), job_id, url);
        let result = self
            .download_video_bytes_internal(url, options, &emitter)
            .await;
        if let Err(error) = &result {
            emitter.emit(ProgressStage::Failed {
                error: error.info(),
            });
        }
        result
    }

    async fn download_video_bytes_internal(
        &self,
        url: &str,
        options: DownloadOptions,
        emitter: &ProgressEmitter,
    ) -> Result<InMemoryVideo> {
        let started = Instant::now();
        let opts = ResolvedOptions::resolve(&self.config, &options);
        emitter.emit(ProgressStage::Resolving);

        let mut resolved = self.resolve_internal(url, &opts, emitter).await?;
        if resolved.info.is_age_restricted {
            return Err(YsttError::AgeRestricted);
        }
        if resolved.info.video_url.is_none() {
            return Err(YsttError::NoVideo);
        }
        emitter.emit(ProgressStage::Resolved {
            post_id: resolved.info.post_id.clone(),
            media_count: 1,
        });

        let mut media_elapsed = std::time::Duration::ZERO;
        let mut re_resolved = false;
        let bytes = loop {
            let video_url = resolved
                .info
                .video_url
                .as_deref()
                .ok_or(YsttError::NoVideo)?;
            let context = MediaContext {
                client: &self.http,
                referer: &resolved.info.page_url,
                cookie: resolved.cookie.as_deref(),
                signal: opts.signal.as_ref(),
                max_bytes: opts.max_bytes,
                read_timeout: self.config.network.http.read_timeout,
                parallel_ranges: opts.parallel_ranges,
                max_connections: opts.max_connections,
                first_chunk: opts.range_first_chunk,
                min_chunk: opts.range_min_chunk,
                range_threshold: opts.range_threshold,
                reuse_existing: false,
                resume: false,
                media_retries: opts.media_retries,
                progress: Some(emitter.clone()),
                adaptive_ranges: opts.adaptive_ranges,
                range_start_connections: opts.range_start_connections,
            };
            let tracker = Some(ProgressTracker::new(
                emitter.clone(),
                MediaTarget::video(),
                None,
            ));
            let attempt_started = Instant::now();
            match download_media_bytes(&context, video_url, MediaTarget::video(), tracker).await {
                Ok(bytes) => {
                    media_elapsed += attempt_started.elapsed();
                    break bytes;
                }
                Err(error) => {
                    media_elapsed += attempt_started.elapsed();
                    if !re_resolved
                        && error.status() == Some(403)
                        && (resolved.from_cache || resolved.from_embed)
                    {
                        if resolved.from_cache {
                            self.cache.invalidate(&resolved.cache_key).await;
                            let _ = self.metadata.invalidate(&resolved.cache_key).await;
                        }
                        resolved = self
                            .resolve_internal_mode(url, &opts, false, emitter)
                            .await?;
                        re_resolved = true;
                        continue;
                    }
                    return Err(error);
                }
            }
        };

        let media_url = resolved.info.video_url.clone().ok_or(YsttError::NoVideo)?;
        let ext = video_ext(&media_url);
        let mime = mime_for_ext(&ext, MediaKind::Video);
        let mut timings = resolved.timings;
        timings.media_ms = millis(media_elapsed);
        timings.total_ms = millis(started.elapsed());
        let video = InMemoryVideo {
            post_id: resolved.info.post_id,
            source_url: resolved.info.source_url,
            page_url: resolved.info.page_url,
            author: resolved.info.author,
            description: resolved.info.description,
            width: resolved.info.video_width,
            height: resolved.info.video_height,
            duration_sec: resolved.info.duration_sec,
            mime,
            ext,
            media_url,
            bytes,
            timings,
        };
        emitter.emit(ProgressStage::Completed {
            total_ms: video.timings.total_ms,
        });
        Ok(video)
    }
}
