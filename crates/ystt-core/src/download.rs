//! End-to-end TikTok resolution and download pipeline.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use futures_util::{StreamExt, stream};
use http::header::LOCATION;
use tokio_util::sync::CancellationToken;
use tracing::debug;
use ystt_net::{BrowserHeaders, HttpClient, NetError, RequestKind, RequestOptions};
use ystt_waf::{WafChallenge, WafSolver};

use crate::cache::UrlCache;
use crate::cache_util::FileLock;
use crate::config::{DownloadOptions, QualityPolicy, YsttConfig};
use crate::cookie::CookieJar;
use crate::error::{PartialFailure, Result, YsttError};
use crate::file_cache::{CachedFile, FileCache, FileCacheEntry};
use crate::media::{
    DownloadedFile, MediaContext, audio_ext, base_filename, download_media, photo_ext, video_ext,
};
use crate::metadata::{MetadataCache, default_metadata_root};
use crate::model::{MediaInfo, MediaKind, TikTokItem};
use crate::parse::{PageParse, parse_embed_page, parse_page};
use crate::progress::{MediaTarget, ProgressEmitter, ProgressStage, next_job_id};
use crate::soundcloud::{
    SoundCloudAudio, SoundCloudClient, SoundCloudEntity, SoundCloudPlaylistAudio,
    SoundCloudTrackFailure,
};
use crate::url::{TikTokUrl, find_post_id_in_html};

use self::job::DownloadJob;
use self::options::ResolvedOptions;

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
            emitter.emit(ProgressStage::Completed { total_ms: 0 });
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
        let opts = ResolvedOptions::resolve(&self.config, options);
        let concurrency = concurrency.clamp(1, 8);
        let track_count = playlist.tracks.len();
        let mut results: Vec<Option<Result<SoundCloudAudio>>> =
            (0..track_count).map(|_| None).collect();
        let mut failures = Vec::new();
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
                failures.push(SoundCloudTrackFailure {
                    track_id: track.id.clone(),
                    title: track.title.clone(),
                    url: track.permalink_url.clone(),
                    error: error.info(),
                });
            }
            results[index] = Some(result);
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
        let base = base_filename(
            opts.filename_template.as_deref(),
            &info.author.id,
            &info.post_id,
            spec,
        );
        let output_dir = opts.output_dir.clone();
        let video_url = info.video_url.clone();
        let audio_url = info.audio_url.clone();

        let video_context = context.clone();
        let video_base = base.clone();
        let video_dir = output_dir.clone();
        let video_future = async move {
            match video_url {
                Some(url) => {
                    let path = video_dir.join(format!("{video_base}.{}", video_ext(&url)));
                    Ok::<_, YsttError>(Some(
                        download_media(
                            &video_context,
                            &url,
                            &path,
                            MediaKind::Video,
                            MediaTarget::video(),
                        )
                        .await?,
                    ))
                }
                None => Ok(None),
            }
        };

        let photo_paths: Vec<(usize, String, PathBuf)> = info
            .photo_urls
            .iter()
            .enumerate()
            .map(|(index, url)| {
                (
                    index,
                    url.clone(),
                    output_dir.join(format!("{base}_photo_{}.{}", index + 1, photo_ext(url))),
                )
            })
            .collect();
        let photo_count = photo_paths.len();

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
        let audio_base = base.clone();
        let audio_dir = output_dir.clone();
        let audio_future = async move {
            if !include_audio {
                return Ok(None);
            }
            match audio_url {
                Some(url) => {
                    let path = audio_dir.join(format!("{audio_base}_audio.{}", audio_ext(&url)));
                    Ok(Some(
                        download_media(
                            &audio_context,
                            &url,
                            &path,
                            MediaKind::Audio,
                            MediaTarget::audio(),
                        )
                        .await?,
                    ))
                }
                None => Ok(None),
            }
        };

        let (video, (photos, failures), audio) =
            tokio::try_join!(video_future, photos_future, audio_future)?;
        Ok((video, photos, audio, failures))
    }

    async fn resolve_internal(
        &self,
        url: &str,
        opts: &ResolvedOptions,
        emitter: &ProgressEmitter,
    ) -> Result<Resolved> {
        self.resolve_internal_mode(url, opts, self.config.resolver.use_embed, emitter)
            .await
    }

    async fn resolve_internal_mode(
        &self,
        url: &str,
        opts: &ResolvedOptions,
        allow_embed: bool,
        emitter: &ProgressEmitter,
    ) -> Result<Resolved> {
        let started = Instant::now();
        let parsed = TikTokUrl::parse(url)?;
        let signal = opts.signal.as_ref();

        check_cancelled(signal)?;

        let post_id = self.resolve_post_id(&parsed, signal).await?;

        let quality = opts.quality;
        let cache_key = format!("{post_id}:{}", quality.as_str());
        let metadata_enabled = opts.metadata_cache;

        if let Some(mut cached) = self.cache.get(&cache_key).await {
            cached.source_url = url.to_owned();
            let elapsed = started.elapsed();
            return Ok(Resolved {
                info: cached,
                timings: Timings {
                    resolve_ms: millis(elapsed),
                    total_ms: millis(elapsed),
                    ..Timings::default()
                },
                from_cache: true,
                from_embed: false,
                cache_key,
                cookie: None,
            });
        }

        if metadata_enabled && let Some(entry) = self.metadata.get(&cache_key).await {
            let mut info = entry.info;
            info.source_url = url.to_owned();
            self.cache.put(cache_key.clone(), info.clone()).await;
            let elapsed = started.elapsed();
            return Ok(Resolved {
                info,
                timings: Timings {
                    resolve_ms: millis(elapsed),
                    total_ms: millis(elapsed),
                    ..Timings::default()
                },
                from_cache: true,
                from_embed: false,
                cache_key,
                cookie: entry.cookie,
            });
        }

        let page_url = parsed.page_url(&post_id);
        let page_url = match &self.config.resolver.page_origin {
            Some(origin) => page_url.replacen("https://www.tiktok.com", origin, 1),
            None => page_url,
        };

        let page_started = Instant::now();
        let prefer_embed = allow_embed
            && parsed.kind() != Some(MediaKind::Photo)
            && quality != QualityPolicy::Best;
        let mut outcome = if prefer_embed {
            match self.fetch_embed(&post_id, signal).await? {
                Some(outcome) => outcome,
                None => {
                    self.fetch_detail(&page_url, &post_id, signal, Some(emitter))
                        .await?
                }
            }
        } else {
            match self
                .fetch_detail(&page_url, &post_id, signal, Some(emitter))
                .await
            {
                Ok(outcome) => outcome,
                Err(primary_error) if parsed.kind() != Some(MediaKind::Photo) => {
                    debug!(?primary_error, "full page failed; trying embed fallback");
                    match self.fetch_embed(&post_id, signal).await {
                        Ok(Some(outcome)) => outcome,
                        _ => return Err(primary_error),
                    }
                }
                Err(error) => return Err(error),
            }
        };
        outcome.page = page_started
            .elapsed()
            .saturating_sub(outcome.waf)
            .saturating_sub(outcome.parse);
        let from_embed = outcome.from_embed;
        let cookie = outcome.cookie.clone();
        let item = outcome.item;

        let video_url = item.best_video_url(quality).map(str::to_owned);
        let photo_entries = item.photo_entries();
        let photo_urls: Vec<String> = photo_entries.iter().map(|(url, _)| url.clone()).collect();
        let photo_dimensions: Vec<_> = photo_entries.iter().map(|(_, dims)| *dims).collect();
        let audio_url = item.audio_url().map(str::to_owned);
        let bitrates = item
            .video
            .as_ref()
            .map(|video| video.bitrates.clone())
            .unwrap_or_default();
        let video_width = item.video.as_ref().and_then(|video| video.width);
        let video_height = item.video.as_ref().and_then(|video| video.height);
        let duration_sec = item.video.as_ref().and_then(|video| video.duration_sec);
        let cover_url = item
            .video
            .as_ref()
            .and_then(|video| video.cover_url.clone());
        let is_slideshow = !item.photos.is_empty();
        // Photo posts also carry a placeholder `video` object with a relative
        // URL. If the post has gallery images, photos are the media.
        let video_url = if photo_urls.is_empty() {
            video_url
        } else {
            None
        };
        let is_age_restricted = item.is_content_classified
            && video_url.is_none()
            && photo_urls.is_empty()
            && audio_url.is_none();

        let info = MediaInfo {
            post_id,
            source_url: url.to_owned(),
            page_url,
            author: item.author,
            description: item.description,
            video_url,
            photo_urls,
            photo_dimensions,
            audio_url,
            is_slideshow,
            is_age_restricted,
            music: item.music,
            bitrates,
            video_width,
            video_height,
            duration_sec,
            cover_url,
        };

        if !info.has_media() && !info.is_age_restricted {
            return Err(YsttError::NoMedia);
        }

        if info.has_media() {
            self.cache.put(cache_key.clone(), info.clone()).await;
            if metadata_enabled
                && let Err(error) = self
                    .metadata
                    .put(&cache_key, &info, outcome.cookie.as_deref())
                    .await
            {
                tracing::warn!(?error, "failed to store metadata cache");
            }
        }

        let elapsed = started.elapsed();
        Ok(Resolved {
            info,
            timings: Timings {
                resolve_ms: millis(elapsed),
                page_ms: millis(outcome.page),
                waf_ms: millis(outcome.waf),
                parse_ms: millis(outcome.parse),
                total_ms: millis(elapsed),
                ..Timings::default()
            },
            from_cache: false,
            from_embed,
            cache_key,
            cookie,
        })
    }

    async fn resolve_post_id(
        &self,
        parsed: &TikTokUrl,
        signal: Option<&CancellationToken>,
    ) -> Result<String> {
        match parsed.post_id() {
            Some(post_id) => Ok(post_id.to_owned()),
            None if parsed.is_short_link() => {
                let short_url = parsed
                    .short_url()
                    .unwrap_or_else(|| parsed.raw().to_owned());
                self.resolve_short_link(&short_url, signal).await
            }
            None => Err(YsttError::Parse(
                "URL does not contain a post ID or short-link code".into(),
            )),
        }
    }

    async fn resolve_short_link(
        &self,
        short_url: &str,
        signal: Option<&CancellationToken>,
    ) -> Result<String> {
        let headers = BrowserHeaders::build(RequestKind::ShortLink, None, None, None)?;
        let options = match signal {
            Some(signal) => RequestOptions::short_link().with_signal(signal.clone()),
            None => RequestOptions::short_link(),
        };
        let response = self.http.get(short_url, headers, options).await?;

        if let Some(location) = response
            .headers()
            .get(LOCATION)
            .and_then(|value| value.to_str().ok())
            && let Some(post_id) = crate::url::get_post_id(location)
        {
            return Ok(post_id);
        }

        let status = response.status_u16();
        if !response.is_success() && !(300..400).contains(&status) {
            return Err(YsttError::Network(NetError::Status {
                status,
                url: short_url.to_owned(),
            }));
        }

        let html = response
            .text(
                self.config.network.max_page_bytes,
                self.config.network.http.read_timeout,
                signal,
            )
            .await?;

        find_post_id_in_html(&html)
            .ok_or_else(|| YsttError::Parse("cannot resolve post ID from short link".into()))
    }

    async fn fetch_embed(
        &self,
        post_id: &str,
        signal: Option<&CancellationToken>,
    ) -> Result<Option<FetchOutcome>> {
        let origin = self
            .config
            .resolver
            .page_origin
            .as_deref()
            .unwrap_or("https://www.tiktok.com")
            .trim_end_matches('/');
        let url = format!("{origin}/embed/v2/{post_id}");
        let headers = BrowserHeaders::build(RequestKind::Page, None, None, None)?;
        let options = match signal {
            Some(signal) => RequestOptions::page().with_signal(signal.clone()),
            None => RequestOptions::page(),
        };

        let started = Instant::now();
        let response = match self.http.get(&url, headers, options).await {
            Ok(response) => response,
            Err(error) => {
                debug!(?error, "embed request failed; falling back to full page");
                return Ok(None);
            }
        };
        if !response.is_success() {
            return Ok(None);
        }

        let mut cookies = CookieJar::default();
        for set_cookie in response.set_cookie_headers() {
            cookies.absorb(&set_cookie);
        }

        let body_started = Instant::now();
        let html = match response
            .text(
                self.config.network.max_page_bytes,
                self.config.network.http.read_timeout,
                signal,
            )
            .await
        {
            Ok(html) => html,
            Err(error) => {
                debug!(?error, "embed body failed; falling back to full page");
                return Ok(None);
            }
        };
        let body_elapsed = body_started.elapsed();
        let parse_started = Instant::now();
        let item = parse_embed_page(&html, post_id);
        let parse_elapsed = parse_started.elapsed();
        debug!(
            headers_ms = started
                .elapsed()
                .saturating_sub(body_elapsed + parse_elapsed)
                .as_millis() as u64,
            body_ms = body_elapsed.as_millis() as u64,
            bytes = html.len(),
            "embed fetched"
        );
        Ok(item.map(|item| FetchOutcome {
            item,
            page: started.elapsed().saturating_sub(parse_elapsed),
            waf: Duration::ZERO,
            parse: parse_elapsed,
            cookie: cookies.header(),
            from_embed: true,
        }))
    }

    async fn fetch_detail(
        &self,
        page_url: &str,
        expected_post_id: &str,
        signal: Option<&CancellationToken>,
        progress: Option<&ProgressEmitter>,
    ) -> Result<FetchOutcome> {
        let fetch_started = Instant::now();
        let mut cookies = CookieJar::default();
        let mut waf_total = Duration::ZERO;
        let mut parse_total = Duration::ZERO;
        let mut last_error: Option<YsttError> = None;

        for attempt in 1..=3u32 {
            check_cancelled(signal)?;

            let cookie = cookies.header();
            let headers = BrowserHeaders::build(RequestKind::Page, None, cookie.as_deref(), None)?;
            let options = match signal {
                Some(signal) => RequestOptions::page().with_signal(signal.clone()),
                None => RequestOptions::page(),
            };
            let request_started = Instant::now();
            let response = self.http.get(page_url, headers, options).await?;
            let headers_elapsed = request_started.elapsed();
            let status = response.status_u16();

            for set_cookie in response.set_cookie_headers() {
                cookies.absorb(&set_cookie);
            }

            if matches!(status, 404 | 410) {
                return Err(YsttError::Unavailable(format!("HTTP {status}")));
            }
            if status == 429 || status >= 500 {
                last_error = Some(YsttError::Network(NetError::Status {
                    status,
                    url: page_url.to_owned(),
                }));
                if attempt < 3 {
                    sleep_or_cancel(Duration::from_millis(250 * u64::from(attempt)), signal)
                        .await?;
                    continue;
                }
                break;
            }

            let version = response.version();
            let body_started = Instant::now();
            let html = response
                .text(
                    self.config.network.max_page_bytes,
                    self.config.network.http.read_timeout,
                    signal,
                )
                .await?;
            debug!(
                attempt,
                ?version,
                headers_ms = headers_elapsed.as_millis() as u64,
                body_ms = body_started.elapsed().as_millis() as u64,
                bytes = html.len(),
                "page fetched"
            );

            let parse_started = Instant::now();
            let parsed = parse_page(&html, expected_post_id);
            parse_total += parse_started.elapsed();

            match parsed? {
                PageParse::Item(item) => {
                    let item = *item;
                    let page_total = fetch_started.elapsed();
                    let page = page_total
                        .saturating_sub(waf_total)
                        .saturating_sub(parse_total);
                    return Ok(FetchOutcome {
                        item,
                        page,
                        waf: waf_total,
                        parse: parse_total,
                        cookie: cookies.header(),
                        from_embed: false,
                    });
                }
                PageParse::NoItem => {
                    if let Some(challenge) =
                        WafChallenge::parse_html(&html, self.config.waf.max_iterations)?
                    {
                        debug!("solving TikTok WAF challenge");
                        let fingerprint = challenge.fingerprint();
                        if let Some(cookie) = self
                            .waf_cookies
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .get(&fingerprint)
                            .cloned()
                        {
                            cookies.set(&cookie);
                            continue;
                        }
                        if let Some(progress) = progress {
                            progress.emit(ProgressStage::WafChallenge { attempt });
                        }
                        let solver = self.waf.clone();
                        let challenge_for_worker = challenge.clone();
                        let solve_started = Instant::now();
                        let cookie = tokio::task::spawn_blocking(move || {
                            solver.solve(&challenge_for_worker)
                        })
                        .await
                        .map_err(|error| YsttError::Internal(error.to_string()))??;
                        let solve_elapsed = solve_started.elapsed();
                        waf_total += solve_elapsed;
                        if let Some(progress) = progress {
                            progress.emit(ProgressStage::WafSolved {
                                elapsed_ms: millis(solve_elapsed),
                            });
                        }
                        {
                            let mut cache = self
                                .waf_cookies
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner());
                            if cache.len() >= 64 {
                                cache.clear();
                            }
                            cache.insert(fingerprint, cookie.clone());
                        }
                        cookies.set(&cookie);
                        continue;
                    }

                    if ystt_waf::looks_like_external_bot_page(&html) {
                        return Err(YsttError::Parse(
                            "external bot/CAPTCHA page is not supported".into(),
                        ));
                    }

                    last_error = Some(if status == 200 {
                        YsttError::Parse("no item payload and no WAF challenge".into())
                    } else {
                        YsttError::Network(NetError::Status {
                            status,
                            url: page_url.to_owned(),
                        })
                    });
                    if attempt < 3 {
                        sleep_or_cancel(Duration::from_millis(250 * u64::from(attempt)), signal)
                            .await?;
                        continue;
                    }
                }
            }
        }

        Err(last_error
            .unwrap_or_else(|| YsttError::Parse("cannot parse TikTok page after retries".into())))
    }
}

mod builder;

pub use builder::YsttBuilder;

struct Resolved {
    info: MediaInfo,
    timings: Timings,
    from_cache: bool,
    from_embed: bool,
    cache_key: String,
    cookie: Option<String>,
}

struct FetchOutcome {
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
