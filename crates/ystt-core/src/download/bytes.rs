//! In-memory media downloads for bots, HTTP APIs and other direct consumers.

use std::time::{Duration, Instant};

use futures_util::{StreamExt, stream};

use crate::config::DownloadOptions;
use crate::error::{PartialFailure, Result, YsttError};
use crate::media::{
    MediaContext, audio_ext, download_media_bytes, mime_for_ext, photo_ext, video_ext,
};
use crate::model::{ImageDimensions, MediaInfo, MediaKind};
use crate::progress::{MediaTarget, ProgressEmitter, ProgressStage, ProgressTracker, next_job_id};

use super::options::ResolvedOptions;
use super::{InMemoryFile, InMemoryPost, InMemoryVideo, Ystt, millis};

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
        let post = self.download_bytes_with(url, options).await?;
        let video = post.video.ok_or(YsttError::NoVideo)?;
        Ok(InMemoryVideo {
            post_id: post.post_id,
            source_url: post.source_url,
            page_url: post.page_url,
            author: post.author,
            description: post.description,
            width: video.width,
            height: video.height,
            duration_sec: video.duration_sec,
            cover_url: post.cover_url,
            mime: video.mime,
            ext: video.ext,
            media_url: video.source_url,
            bytes: video.bytes,
            timings: post.timings,
        })
    }

    /// Download every media item into memory with default options.
    pub async fn download_bytes(&self, url: &str) -> Result<InMemoryPost> {
        self.download_bytes_with(url, DownloadOptions::default())
            .await
    }

    /// Download every media item into memory with per-call overrides.
    ///
    /// Videos, galleries and optional music are kept as `Bytes`; no media or
    /// file-cache entry is written to disk.
    pub async fn download_bytes_with(
        &self,
        url: &str,
        options: DownloadOptions,
    ) -> Result<InMemoryPost> {
        let job_id = next_job_id();
        let emitter = ProgressEmitter::single(options.progress.clone(), job_id, url);
        let result = self.download_bytes_internal(url, options, &emitter).await;
        if let Err(error) = &result {
            emitter.emit(ProgressStage::Failed {
                error: error.info(),
            });
        }
        result
    }

    async fn download_bytes_internal(
        &self,
        url: &str,
        options: DownloadOptions,
        emitter: &ProgressEmitter,
    ) -> Result<InMemoryPost> {
        let started = Instant::now();
        let opts = ResolvedOptions::resolve(&self.config, &options);
        emitter.emit(ProgressStage::Resolving);

        let mut resolved = self.resolve_internal(url, &opts, emitter).await?;
        if resolved.info.is_age_restricted {
            return Err(YsttError::AgeRestricted);
        }
        if !resolved.info.has_media() {
            return Err(YsttError::NoMedia);
        }
        emitter.emit(ProgressStage::Resolved {
            post_id: resolved.info.post_id.clone(),
            media_count: resolved.info.video_url.is_some() as usize
                + resolved.info.photo_urls.len()
                + (opts.include_audio && resolved.info.audio_url.is_some()) as usize,
        });

        let mut media_elapsed = Duration::ZERO;
        let mut re_resolved = false;
        let (video, photos, audio, failures) = loop {
            let attempt_started = Instant::now();
            match self
                .transfer_bytes(&resolved.info, &opts, resolved.cookie.as_deref(), emitter)
                .await
            {
                Ok(files) => {
                    media_elapsed += attempt_started.elapsed();
                    break files;
                }
                Err(error) => {
                    media_elapsed += attempt_started.elapsed();
                    if !re_resolved
                        && error.status() == Some(403)
                        && (resolved.from_cache || resolved.from_embed)
                    {
                        debug_re_resolve(&resolved);
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

        if !opts.allow_partial && !failures.is_empty() {
            return Err(YsttError::Partial(failures));
        }
        if video.is_none() && photos.is_empty() && audio.is_none() {
            return if failures.is_empty() {
                Err(YsttError::NoMedia)
            } else {
                Err(YsttError::Partial(failures))
            };
        }

        let mut timings = resolved.timings;
        timings.media_ms = millis(media_elapsed);
        timings.total_ms = millis(started.elapsed());
        let post = InMemoryPost {
            post_id: resolved.info.post_id,
            source_url: resolved.info.source_url,
            page_url: resolved.info.page_url,
            author: resolved.info.author,
            description: resolved.info.description,
            cover_url: resolved.info.cover_url,
            video,
            photos,
            audio,
            failures,
            timings,
        };
        emitter.emit(ProgressStage::Completed {
            total_ms: post.timings.total_ms,
        });
        Ok(post)
    }

    async fn transfer_bytes(
        &self,
        info: &MediaInfo,
        opts: &ResolvedOptions,
        cookie: Option<&str>,
        emitter: &ProgressEmitter,
    ) -> Result<(
        Option<InMemoryFile>,
        Vec<InMemoryFile>,
        Option<InMemoryFile>,
        Vec<PartialFailure>,
    )> {
        let context = MediaContext {
            client: &self.http,
            referer: &info.page_url,
            cookie,
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

        let video_url = info.video_url.clone();
        let video_context = context.clone();
        let video_future = async move {
            let Some(url) = video_url else {
                return Ok::<_, YsttError>(None);
            };
            let tracker = ProgressTracker::new(
                video_context
                    .progress
                    .clone()
                    .expect("media progress emitter"),
                MediaTarget::video(),
                None,
            );
            let bytes =
                download_media_bytes(&video_context, &url, MediaTarget::video(), Some(tracker))
                    .await?;
            Ok(Some(InMemoryFile {
                bytes,
                mime: mime_for_ext(&video_ext(&url), MediaKind::Video),
                ext: video_ext(&url),
                size: 0,
                source_url: url,
                kind: MediaKind::Video,
                width: info.video_width,
                height: info.video_height,
                duration_sec: info.duration_sec,
            }))
        };

        let photo_urls = info.photo_urls.clone();
        let photo_dimensions = info.photo_dimensions.clone();
        let photo_context = context.clone();
        let photo_concurrency = opts.photo_concurrency.max(1);
        let photos_future = async move {
            let photo_count = photo_urls.len();
            let mut results: Vec<Option<InMemoryFile>> = vec![None; photo_count];
            let mut failures = Vec::new();
            let mut downloads =
                stream::iter(photo_urls.into_iter().enumerate().map(move |(index, url)| {
                    let context = photo_context.clone();
                    let dimensions = photo_dimensions.get(index).copied().flatten();
                    async move {
                        let tracker = ProgressTracker::new(
                            context.progress.clone().expect("media progress emitter"),
                            MediaTarget::photo(index),
                            None,
                        );
                        let result = download_media_bytes(
                            &context,
                            &url,
                            MediaTarget::photo(index),
                            Some(tracker),
                        )
                        .await;
                        (index, url, dimensions, result)
                    }
                }))
                .buffer_unordered(photo_concurrency);
            while let Some((index, url, dimensions, result)) = downloads.next().await {
                match result {
                    Ok(bytes) => {
                        let ImageDimensions { width, height } =
                            dimensions.unwrap_or(ImageDimensions {
                                width: 0,
                                height: 0,
                            });
                        results[index] = Some(InMemoryFile {
                            mime: mime_for_ext(&photo_ext(&url), MediaKind::Photo),
                            ext: photo_ext(&url),
                            size: bytes.len() as u64,
                            source_url: url,
                            kind: MediaKind::Photo,
                            width: (width > 0).then_some(width),
                            height: (height > 0).then_some(height),
                            duration_sec: None,
                            bytes,
                        });
                    }
                    Err(error) => failures.push(PartialFailure {
                        kind: MediaKind::Photo,
                        url,
                        error: error.to_string(),
                    }),
                }
            }
            Ok::<_, YsttError>((results.into_iter().flatten().collect(), failures))
        };

        let audio_url = info.audio_url.clone();
        let audio_context = context.clone();
        let include_audio = opts.include_audio;
        let audio_future = async move {
            if !include_audio {
                return Ok::<_, YsttError>((None, None));
            }
            let Some(url) = audio_url else {
                return Ok((None, None));
            };
            let tracker = ProgressTracker::new(
                audio_context
                    .progress
                    .clone()
                    .expect("media progress emitter"),
                MediaTarget::audio(),
                None,
            );
            match download_media_bytes(&audio_context, &url, MediaTarget::audio(), Some(tracker))
                .await
            {
                Ok(bytes) => Ok((
                    Some(InMemoryFile {
                        mime: mime_for_ext(&audio_ext(&url), MediaKind::Audio),
                        ext: audio_ext(&url),
                        size: bytes.len() as u64,
                        source_url: url,
                        kind: MediaKind::Audio,
                        width: None,
                        height: None,
                        duration_sec: None,
                        bytes,
                    }),
                    None,
                )),
                Err(error) => Ok((
                    None,
                    Some(PartialFailure {
                        kind: MediaKind::Audio,
                        url,
                        error: error.to_string(),
                    }),
                )),
            }
        };

        let (video, (photos, photo_failures), (audio, audio_failure)) =
            tokio::try_join!(video_future, photos_future, audio_future)?;
        let mut failures = photo_failures;
        if let Some(failure) = audio_failure {
            failures.push(failure);
        }
        let video = video.map(|mut file| {
            file.size = file.bytes.len() as u64;
            file
        });
        Ok((video, photos, audio, failures))
    }
}

fn debug_re_resolve(resolved: &super::Resolved) {
    tracing::debug!(
        cache_key = %resolved.cache_key,
        from_cache = resolved.from_cache,
        from_embed = resolved.from_embed,
        "media returned 403; re-resolving with the full page"
    );
}
