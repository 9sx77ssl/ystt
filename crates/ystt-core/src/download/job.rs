//! Single-download state machine.

use std::time::{Duration, Instant};

use tracing::{debug, info};

use crate::config::DownloadOptions;
use crate::error::{Result, YsttError};
use crate::progress::{ProgressEmitter, ProgressStage};

use super::options::ResolvedOptions;
use super::{DownloadResult, Ystt, file_cache_key, millis};

/// One download job: cache lookup, resolve, transfer, cache store, timings.
pub(crate) struct DownloadJob<'a> {
    ystt: &'a Ystt,
    url: &'a str,
    opts: ResolvedOptions,
    emitter: ProgressEmitter,
    started: Instant,
}

impl<'a> DownloadJob<'a> {
    pub(crate) fn new(
        ystt: &'a Ystt,
        url: &'a str,
        options: DownloadOptions,
        emitter: ProgressEmitter,
    ) -> Self {
        let opts = ResolvedOptions::resolve(&ystt.config, &options);
        Self {
            ystt,
            url,
            opts,
            emitter,
            started: Instant::now(),
        }
    }

    pub(crate) async fn run(self) -> Result<DownloadResult> {
        let started = self.started;
        let url = self.url;
        let opts = &self.opts;
        let emitter = &self.emitter;

        emitter.emit(ProgressStage::Resolving);

        let (cached, cache_lock) = if opts.file_cache {
            self.ystt.try_file_cache(url, opts, emitter).await?
        } else {
            (None, None)
        };
        if let Some(result) = cached {
            emitter.emit(ProgressStage::Completed {
                total_ms: result.timings.total_ms,
            });
            return Ok(result);
        }

        let mut resolved = self.ystt.resolve_internal(url, opts, emitter).await?;

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
                .ystt
                .transfer(&resolved.info, opts, resolved.cookie.as_deref(), emitter)
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
                        debug!("media returned 403; re-resolving with the full page");
                        if resolved.from_cache {
                            self.ystt.cache.invalidate(&resolved.cache_key).await;
                            let _ = self.ystt.metadata.invalidate(&resolved.cache_key).await;
                        }
                        resolved = self
                            .ystt
                            .resolve_internal_mode(url, opts, false, emitter)
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

        if opts.file_cache && failures.is_empty() {
            let key = file_cache_key(&resolved.info.post_id, opts.quality, opts.include_audio);
            if let Err(error) = self
                .ystt
                .store_file_cache(
                    &key,
                    &resolved.info,
                    &video,
                    &photos,
                    &audio,
                    &opts.output_dir,
                )
                .await
            {
                tracing::warn!(?error, "failed to store file cache");
            }
        }
        drop(cache_lock);

        info!(
            post_id = %resolved.info.post_id,
            video = video.is_some(),
            photos = photos.len(),
            audio = audio.is_some(),
            failures = failures.len(),
            total_ms = timings.total_ms,
            "download complete"
        );

        let result = DownloadResult {
            post_id: resolved.info.post_id,
            source_url: resolved.info.source_url,
            page_url: resolved.info.page_url,
            author: resolved.info.author,
            description: resolved.info.description,
            video,
            photos,
            audio,
            failures,
            cache_hit: false,
            timings,
        };
        emitter.emit(ProgressStage::Completed {
            total_ms: result.timings.total_ms,
        });
        Ok(result)
    }
}
