//! End-to-end TikTok resolution and download pipeline.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use futures_util::{StreamExt, stream};
use tokio_util::sync::CancellationToken;
use ystt_net::HttpClient;
use ystt_waf::WafSolver;

use crate::cache::UrlCache;
use crate::cache_util::FileLock;
use crate::config::{DownloadOptions, FilenameCollision, QualityPolicy, YsttConfig};
use crate::error::{PartialFailure, Result, YsttError};
use crate::file_cache::{CachedFile, FileCache, FileCacheEntry};
use crate::media::{
    DownloadedFile, MediaContext, audio_ext, download_media, media_filename, photo_ext, video_ext,
};
use crate::metadata::{MetadataCache, default_metadata_root};
use crate::model::{MediaInfo, MediaKind, TikTokItem};
use crate::progress::{MediaTarget, ProgressEmitter, ProgressStage, next_job_id};
use crate::soundcloud::{
    SoundCloudAudio, SoundCloudClient, SoundCloudEntity, SoundCloudPlaylistAudio,
    SoundCloudTrackFailure,
};
use crate::url::TikTokUrl;

use self::job::DownloadJob;
use self::options::ResolvedOptions;

mod resolver;

mod batch;
mod bytes;
mod job;
mod options;
mod types;

pub use types::{
    BatchItemResult, BatchOptions, BatchResult, DownloadResult, InMemoryFile, InMemoryPost,
    InMemoryVideo, Timings,
};

/// Main public downloader facade.
#[derive(Clone)]
pub struct Ystt {
    config: YsttConfig,
    http: HttpClient,
    waf: WafSolver,
    cache: std::sync::Arc<UrlCache>,
    metadata: MetadataCache,
    soundcloud: SoundCloudClient,
    waf_cookies: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, String>>>,
}

impl std::fmt::Debug for Ystt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Ystt")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl Ystt {
    /// Start building a downloader.
    pub fn builder() -> YsttBuilder {
        YsttBuilder::default()
    }

    /// Construct from a complete configuration.
    pub fn new(config: YsttConfig) -> Result<Self> {
        let http = HttpClient::new(config.network.http.clone())?;
        let soundcloud = SoundCloudClient::new(
            http.clone(),
            config.network.max_page_bytes,
            config.network.http.read_timeout,
        );
        let waf = WafSolver::new(config.waf.clone());
        let cache = std::sync::Arc::new(UrlCache::new(config.cache.url_ttl));
        let metadata_root = config
            .cache
            .metadata_dir
            .clone()
            .or_else(|| config.cache.dir.as_ref().map(|root| root.join("metadata")))
            .unwrap_or_else(default_metadata_root);
        let metadata = MetadataCache::new(metadata_root, config.cache.metadata_ttl);
        Ok(Self {
            config,
            http,
            waf,
            cache,
            metadata,
            soundcloud,
            waf_cookies: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
        })
    }

    /// Effective configuration.
    pub fn config(&self) -> &YsttConfig {
        &self.config
    }

    /// Shared SoundCloud resolver/client.
    pub fn soundcloud(&self) -> &SoundCloudClient {
        &self.soundcloud
    }

    /// Resolve a public SoundCloud track or playlist.
    pub async fn resolve_soundcloud(&self, url: &str) -> Result<SoundCloudEntity> {
        self.soundcloud.resolve(url).await
    }

    /// Resolve a public SoundCloud track or playlist with cancellation support.
    pub async fn resolve_soundcloud_with(
        &self,
        url: &str,
        options: &DownloadOptions,
    ) -> Result<SoundCloudEntity> {
        let opts = ResolvedOptions::resolve(&self.config, options);
        self.soundcloud
            .resolve_with_signal(url, opts.signal.as_ref())
            .await
    }

    /// Download one public SoundCloud track into memory with default options.
    pub async fn download_soundcloud_track(&self, url: &str) -> Result<SoundCloudAudio> {
        self.download_soundcloud_track_with(url, DownloadOptions::default())
            .await
    }

    /// Download one public SoundCloud track into memory.
    pub async fn download_soundcloud_track_with(
        &self,
        url: &str,
        options: DownloadOptions,
    ) -> Result<SoundCloudAudio> {
        let opts = ResolvedOptions::resolve(&self.config, &options);
        let job_id = next_job_id();
        let emitter = ProgressEmitter::single(opts.progress.clone(), job_id, url);
        let started = Instant::now();
        emitter.emit(ProgressStage::Resolving);
        let result = async {
            let entity = self
                .soundcloud
                .resolve_with_signal(url, opts.signal.as_ref())
                .await?;
            let track = match entity {
                SoundCloudEntity::Track(track) => track,
                SoundCloudEntity::Playlist(_) => {
                    return Err(YsttError::UnsupportedUrl(
                        "SoundCloud playlist URL; resolve the playlist and download its tracks"
                            .into(),
                    ));
                }
            };
            emitter.emit(ProgressStage::Resolved {
                post_id: track.id.clone(),
                media_count: 1,
            });
            let audio = self
                .soundcloud
                .download_track(
                    &track,
                    opts.quality,
                    opts.max_bytes,
                    opts.max_connections,
                    opts.media_retries,
                    opts.signal.as_ref(),
                )
                .await?;
            emitter.emit(ProgressStage::Completed {
                total_ms: millis(started.elapsed()),
            });
            Ok(audio)
        }
        .await;
        if let Err(error) = &result {
            emitter.emit(ProgressStage::Failed {
                error: error.info(),
            });
        }
        result
    }

    /// Download one already-resolved SoundCloud track into memory.
    pub async fn download_soundcloud_resolved_track(
        &self,
        track: &crate::soundcloud::SoundCloudTrack,
        options: &DownloadOptions,
    ) -> Result<SoundCloudAudio> {
        let opts = ResolvedOptions::resolve(&self.config, options);
        let job_id = next_job_id();
        let emitter = ProgressEmitter::single(opts.progress.clone(), job_id, &track.permalink_url);
        let result = self
            .soundcloud
            .download_track(
                track,
                opts.quality,
                opts.max_bytes,
                opts.max_connections,
                opts.media_retries,
                opts.signal.as_ref(),
            )
            .await;
        if let Err(error) = &result {
            emitter.emit(ProgressStage::Failed {
                error: error.info(),
            });
        }
        result
    }

    /// Download every track of a public SoundCloud playlist into memory.
    ///
    /// Tracks are returned in playlist order. A per-track failure is reported
    /// when `allow_partial` is enabled; otherwise the first failure aborts the
    /// playlist with [`YsttError::Partial`].
    pub async fn download_soundcloud_playlist_with(
        &self,
        url: &str,
        options: DownloadOptions,
        concurrency: usize,
    ) -> Result<SoundCloudPlaylistAudio> {
        let entity = self.resolve_soundcloud_with(url, &options).await?;
        let playlist = match entity {
            SoundCloudEntity::Playlist(playlist) => playlist,
            SoundCloudEntity::Track(_) => {
                return Err(YsttError::UnsupportedUrl(
                    "SoundCloud track URL; not a playlist".into(),
                ));
            }
        };
        self.download_soundcloud_resolved_playlist(&playlist, &options, concurrency)
            .await
    }

    /// Download an already-resolved public SoundCloud playlist into memory.
    pub async fn download_soundcloud_resolved_playlist(
        &self,
        playlist: &crate::soundcloud::SoundCloudPlaylist,
        options: &DownloadOptions,
        concurrency: usize,
    ) -> Result<SoundCloudPlaylistAudio> {
        let started = Instant::now();
        let opts = ResolvedOptions::resolve(&self.config, options);
        let concurrency = concurrency.clamp(1, 8);
        let track_count = playlist.tracks.len();
        let job_id = next_job_id();
        let progress = ProgressEmitter::batch(opts.progress.clone(), job_id, track_count);
        progress.emit(ProgressStage::BatchStarted {
            total_items: track_count,
        });
        let mut results: Vec<Option<Result<SoundCloudAudio>>> =
            (0..track_count).map(|_| None).collect();
        let mut failures = Vec::new();
        let mut completed_tracks = 0usize;
        let mut failed_tracks = 0usize;
        let mut downloads = stream::iter(playlist.tracks.iter().cloned().enumerate().map(
            |(index, track)| {
                let client = self.soundcloud.clone();
                let opts = opts.clone();
                async move {
                    let result = client
                        .download_track(
                            &track,
                            opts.quality,
                            opts.max_bytes,
                            opts.max_connections.min(4),
                            opts.media_retries,
                            opts.signal.as_ref(),
                        )
                        .await;
                    (index, track, result)
                }
            },
        ))
        .buffer_unordered(concurrency);
        while let Some((index, track, result)) = downloads.next().await {
            if let Err(error) = &result {
                failed_tracks += 1;
                failures.push(SoundCloudTrackFailure {
                    track_id: track.id.clone(),
                    title: track.title.clone(),
                    url: track.permalink_url.clone(),
                    error: error.info(),
                });
            } else {
                completed_tracks += 1;
            }
            results[index] = Some(result);
            progress.emit(ProgressStage::BatchProgress {
                completed_items: completed_tracks,
                failed_items: failed_tracks,
                total_items: track_count,
            });
        }
        drop(downloads);
        if !opts.allow_partial && !failures.is_empty() {
            return Err(YsttError::Partial(
                failures
                    .iter()
                    .map(|failure| PartialFailure {
                        kind: MediaKind::Audio,
                        url: failure.url.clone(),
                        error: failure.error.message.clone(),
                    })
                    .collect(),
            ));
        }
        let tracks = results
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .collect::<Vec<_>>();
        if tracks.is_empty() {
            return Err(YsttError::Partial(
                failures
                    .iter()
                    .map(|failure| PartialFailure {
                        kind: MediaKind::Audio,
                        url: failure.url.clone(),
                        error: failure.error.message.clone(),
                    })
                    .collect(),
            ));
        }
        progress.emit(ProgressStage::BatchCompleted {
            total_ms: millis(started.elapsed()),
            successes: completed_tracks,
            failures: failed_tracks,
        });
        Ok(SoundCloudPlaylistAudio {
            playlist: playlist.clone(),
            tracks,
            failures,
        })
    }

    /// Resolve metadata without downloading media.
    pub async fn resolve(&self, url: &str) -> Result<MediaInfo> {
        self.resolve_with(url, &DownloadOptions::default()).await
    }

    /// Resolve with per-call overrides.
    pub async fn resolve_with(&self, url: &str, options: &DownloadOptions) -> Result<MediaInfo> {
        let opts = ResolvedOptions::resolve(&self.config, options);
        let job_id = next_job_id();
        let emitter = ProgressEmitter::single(opts.progress.clone(), job_id, url);
        emitter.emit(ProgressStage::Resolving);
        let result = self.resolve_internal(url, &opts, &emitter).await;
        match &result {
            Ok(info) => emitter.emit(ProgressStage::Resolved {
                post_id: info.info.post_id.clone(),
                media_count: info.info.video_url.is_some() as usize
                    + info.info.photo_urls.len()
                    + info.info.audio_url.is_some() as usize,
            }),
            Err(error) => emitter.emit(ProgressStage::Failed {
                error: error.info(),
            }),
        }
        result.map(|resolved| resolved.info)
    }

    /// Download a post with default options.
    pub async fn download(&self, url: &str) -> Result<DownloadResult> {
        self.download_with(url, DownloadOptions::default()).await
    }

    /// Download a post with per-call overrides.
    pub async fn download_with(
        &self,
        url: &str,
        options: DownloadOptions,
    ) -> Result<DownloadResult> {
        let job_id = next_job_id();
        let emitter = ProgressEmitter::single(options.progress.clone(), job_id, url);
        let result = self.download_with_emitter(url, options, &emitter).await;
        if let Err(error) = &result {
            emitter.emit(ProgressStage::Failed {
                error: error.info(),
            });
        }
        result
    }

    async fn download_with_emitter(
        &self,
        url: &str,
        options: DownloadOptions,
        emitter: &ProgressEmitter,
    ) -> Result<DownloadResult> {
        DownloadJob::new(self, url, options, emitter.clone())
            .run()
            .await
    }

    async fn try_file_cache(
        &self,
        url: &str,
        opts: &ResolvedOptions,
        emitter: &ProgressEmitter,
    ) -> Result<(Option<DownloadResult>, Option<FileLock>)> {
        let started = Instant::now();
        let parsed = TikTokUrl::parse(url)?;
        let signal = opts.signal.as_ref();
        let post_id = self.resolve_post_id(&parsed, signal).await?;
        let key = file_cache_key(&post_id, opts.quality, opts.include_audio);
        let cache = FileCache::new(
            self.file_cache_root(&opts.output_dir),
            self.config.cache.file_ttl,
        );
        let lock = cache.lock(&key).await?;

        let Some(entry) = cache.get(&key).await else {
            return Ok((None, Some(lock)));
        };

        let materialize_started = Instant::now();
        let video = match &entry.video {
            Some(cached) => match materialize_cached(&cache, cached, &opts.output_dir).await {
                Ok(file) => Some(file),
                Err(error) => {
                    tracing::warn!(?error, "failed to materialize cached video");
                    return Ok((None, Some(lock)));
                }
            },
            None => None,
        };
        let mut photos = Vec::with_capacity(entry.photos.len());
        for cached in &entry.photos {
            match materialize_cached(&cache, cached, &opts.output_dir).await {
                Ok(file) => photos.push(file),
                Err(error) => {
                    tracing::warn!(?error, "failed to materialize cached photo");
                    return Ok((None, Some(lock)));
                }
            }
        }
        let audio = match &entry.audio {
            Some(cached) => match materialize_cached(&cache, cached, &opts.output_dir).await {
                Ok(file) => Some(file),
                Err(error) => {
                    tracing::warn!(?error, "failed to materialize cached audio");
                    return Ok((None, Some(lock)));
                }
            },
            None => None,
        };

        let elapsed = started.elapsed();
        let media_elapsed = materialize_started.elapsed();
        let mut info = entry.info;
        info.source_url = url.to_owned();
        emitter.emit(ProgressStage::CacheHit {
            post_id: info.post_id.clone(),
        });

        Ok((
            Some(DownloadResult {
                post_id: info.post_id,
                source_url: info.source_url,
                page_url: info.page_url,
                author: info.author,
                description: info.description,
                video,
                photos,
                audio,
                failures: Vec::new(),
                cache_hit: true,
                timings: Timings {
                    resolve_ms: millis(elapsed),
                    media_ms: millis(media_elapsed),
                    total_ms: millis(elapsed),
                    ..Timings::default()
                },
            }),
            None,
        ))
    }

    async fn store_file_cache(
        &self,
        key: &str,
        info: &MediaInfo,
        video: &Option<DownloadedFile>,
        photos: &[DownloadedFile],
        audio: &Option<DownloadedFile>,
        output_dir: &Path,
    ) -> Result<()> {
        let cache = FileCache::new(self.file_cache_root(output_dir), self.config.cache.file_ttl);
        let video = match video {
            Some(file) => Some(cache.cached_file(file).await?),
            None => None,
        };
        let mut cached_photos = Vec::with_capacity(photos.len());
        for photo in photos {
            cached_photos.push(cache.cached_file(photo).await?);
        }
        let audio = match audio {
            Some(file) => Some(cache.cached_file(file).await?),
            None => None,
        };
        cache
            .put(
                key,
                FileCacheEntry {
                    key: key.to_owned(),
                    info: info.clone(),
                    video,
                    photos: cached_photos,
                    audio,
                    created_unix: 0,
                },
            )
            .await
    }

    fn file_cache_root(&self, output_dir: &Path) -> PathBuf {
        self.config
            .cache
            .file_dir
            .clone()
            .or_else(|| {
                self.config
                    .cache
                    .dir
                    .as_ref()
                    .map(|root| root.join("files"))
            })
            .unwrap_or_else(|| output_dir.join(".ystt-cache"))
    }

    async fn transfer(
        &self,
        info: &MediaInfo,
        opts: &ResolvedOptions,
        cookie: Option<&str>,
        emitter: &ProgressEmitter,
    ) -> Result<(
        Option<DownloadedFile>,
        Vec<DownloadedFile>,
        Option<DownloadedFile>,
        Vec<PartialFailure>,
    )> {
        let parallel_ranges = opts.parallel_ranges;
        let max_connections = opts.max_connections;
        let photo_concurrency = opts.photo_concurrency;
        let max_bytes = opts.max_bytes;
        let include_audio = opts.include_audio;
        let reuse_existing = opts.reuse_existing;
        let resume = opts.resume;
        let media_retries = opts.media_retries;
        let quality = opts.quality;

        let context = MediaContext {
            client: &self.http,
            referer: &info.page_url,
            cookie,
            signal: opts.signal.as_ref(),
            max_bytes,
            read_timeout: self.config.network.http.read_timeout,
            parallel_ranges,
            max_connections,
            first_chunk: opts.range_first_chunk,
            min_chunk: opts.range_min_chunk,
            range_threshold: opts.range_threshold,
            reuse_existing,
            resume,
            media_retries,
            progress: Some(emitter.clone()),
            adaptive_ranges: opts.adaptive_ranges,
            range_start_connections: opts.range_start_connections,
        };

        let spec = quality.as_str();
        let output_dir = opts.output_dir.clone();
        let video_url = info.video_url.clone();
        let audio_url = info.audio_url.clone();
        let template = opts.filename_template.as_deref();
        let collision = opts.filename_collision;
        let mut reserved = std::collections::HashSet::new();

        let video_path = match &video_url {
            Some(url) => {
                let ext = video_ext(url);
                let path = output_dir.join(media_filename(
                    template,
                    &info.author.id,
                    &info.post_id,
                    spec,
                    MediaKind::Video,
                    None,
                    &ext,
                ));
                Some(planned_output_path(path, collision, &mut reserved).await?)
            }
            None => None,
        };

        let mut photo_paths: Vec<(usize, String, PathBuf)> =
            Vec::with_capacity(info.photo_urls.len());
        for (index, url) in info.photo_urls.iter().enumerate() {
            let ext = photo_ext(url);
            let path = output_dir.join(media_filename(
                template,
                &info.author.id,
                &info.post_id,
                spec,
                MediaKind::Photo,
                Some(index + 1),
                &ext,
            ));
            photo_paths.push((
                index,
                url.clone(),
                planned_output_path(path, collision, &mut reserved).await?,
            ));
        }
        let photo_count = photo_paths.len();

        let audio_path = match &audio_url {
            Some(url) if include_audio => {
                let ext = audio_ext(url);
                let path = output_dir.join(media_filename(
                    template,
                    &info.author.id,
                    &info.post_id,
                    spec,
                    MediaKind::Audio,
                    None,
                    &ext,
                ));
                Some(planned_output_path(path, collision, &mut reserved).await?)
            }
            _ => None,
        };

        let video_context = context.clone();
        let video_future = async move {
            match (video_url, video_path) {
                (Some(url), Some(path)) => Ok::<_, YsttError>(Some(
                    download_media(
                        &video_context,
                        &url,
                        &path,
                        MediaKind::Video,
                        MediaTarget::video(),
                    )
                    .await?,
                )),
                _ => Ok(None),
            }
        };

        let photo_context = context.clone();
        let photos_future = async move {
            let mut results: Vec<Option<DownloadedFile>> = vec![None; photo_count];
            let mut failures = Vec::new();
            let mut downloads =
                stream::iter(photo_paths.into_iter().map(move |(index, url, path)| {
                    let context = photo_context.clone();
                    async move {
                        let file = download_media(
                            &context,
                            &url,
                            &path,
                            MediaKind::Photo,
                            MediaTarget::photo(index),
                        )
                        .await;
                        (index, url, file)
                    }
                }))
                .buffer_unordered(photo_concurrency);

            while let Some((index, url, result)) = downloads.next().await {
                match result {
                    Ok(file) => results[index] = Some(file),
                    Err(error) => {
                        failures.push(PartialFailure {
                            kind: MediaKind::Photo,
                            url,
                            error: error.to_string(),
                        });
                    }
                }
            }

            Ok::<_, YsttError>((results.into_iter().flatten().collect(), failures))
        };

        let audio_context = context.clone();
        let audio_future = async move {
            match (audio_url, audio_path) {
                (Some(url), Some(path)) => Ok(Some(
                    download_media(
                        &audio_context,
                        &url,
                        &path,
                        MediaKind::Audio,
                        MediaTarget::audio(),
                    )
                    .await?,
                )),
                _ => Ok(None),
            }
        };

        let (video, (photos, failures), audio) =
            tokio::try_join!(video_future, photos_future, audio_future)?;
        Ok((video, photos, audio, failures))
    }
}

mod builder;

pub use builder::YsttBuilder;

pub(crate) struct Resolved {
    info: MediaInfo,
    timings: Timings,
    from_cache: bool,
    from_embed: bool,
    cache_key: String,
    cookie: Option<String>,
}

pub(crate) struct FetchOutcome {
    item: TikTokItem,
    page: Duration,
    waf: Duration,
    parse: Duration,
    cookie: Option<String>,
    from_embed: bool,
}

fn check_cancelled(signal: Option<&CancellationToken>) -> Result<()> {
    match signal {
        Some(signal) if signal.is_cancelled() => Err(YsttError::Cancelled),
        _ => Ok(()),
    }
}

async fn sleep_or_cancel(duration: Duration, signal: Option<&CancellationToken>) -> Result<()> {
    match signal {
        Some(signal) => {
            tokio::select! {
                _ = signal.cancelled() => Err(YsttError::Cancelled),
                _ = tokio::time::sleep(duration) => Ok(()),
            }
        }
        None => {
            tokio::time::sleep(duration).await;
            Ok(())
        }
    }
}

fn millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

async fn planned_output_path(
    path: PathBuf,
    collision: FilenameCollision,
    reserved: &mut std::collections::HashSet<PathBuf>,
) -> Result<PathBuf> {
    if collision == FilenameCollision::Overwrite {
        return Ok(path);
    }
    let mut candidate = path.clone();
    let mut counter = 0u32;
    loop {
        let exists = tokio::fs::try_exists(&candidate).await?;
        if !exists && reserved.insert(candidate.clone()) {
            return Ok(candidate);
        }
        if collision == FilenameCollision::Fail {
            return Err(YsttError::FileExists(candidate));
        }
        counter += 1;
        if counter > 10_000 {
            return Err(YsttError::Config(
                "could not find a free output filename".into(),
            ));
        }
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("");
        let stem = path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("download");
        candidate = path.with_file_name(if extension.is_empty() {
            format!("{stem}_{counter}")
        } else {
            format!("{stem}_{counter}.{extension}")
        });
    }
}

fn file_cache_key(post_id: &str, quality: QualityPolicy, include_audio: bool) -> String {
    format!("{post_id}:{}:{}", quality.as_str(), u8::from(include_audio))
}

async fn materialize_cached(
    cache: &FileCache,
    cached: &CachedFile,
    output_dir: &Path,
) -> Result<DownloadedFile> {
    let destination = output_dir.join(&cached.filename);
    cache.materialize(&cached.file.path, &destination).await?;
    let mut file = cached.file.clone();
    file.path = destination;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn millis_are_saturated() {
        assert_eq!(millis(Duration::from_millis(42)), 42);
    }
}
