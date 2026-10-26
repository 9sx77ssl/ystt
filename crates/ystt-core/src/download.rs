//! End-to-end TikTok resolution and download pipeline.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use futures_util::{StreamExt, stream};
use http::header::LOCATION;
use serde::Serialize;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};
use ystt_net::{BrowserHeaders, HttpClient, NetError, RequestKind, RequestOptions};
use ystt_waf::{WafChallenge, WafSolver};

use crate::cache::UrlCache;
use crate::config::{DownloadOptions, QualityPolicy, YsttConfig};
use crate::error::{PartialFailure, Result, YsttError};
use crate::file_cache::{CachedFile, FileCache, FileCacheEntry};
use crate::media::{
    DownloadedFile, MediaContext, audio_ext, base_filename, download_media, photo_ext, video_ext,
};
use crate::model::{MediaInfo, MediaKind, TikTokItem};
use crate::parse::{PageParse, parse_embed_page, parse_page};
use crate::progress::{MediaTarget, ProgressEvent, ProgressReporter};
use crate::url::{TikTokUrl, find_post_id_in_html};

/// Stage timings for one download.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Timings {
    /// Full resolve stage (URL parsing, short link, page fetch).
    pub resolve_ms: u64,
    /// Page HTTP and body time.
    pub page_ms: u64,
    /// WAF challenge solving time.
    pub waf_ms: u64,
    /// JSON parsing time.
    pub parse_ms: u64,
    /// Media transfer time.
    pub media_ms: u64,
    /// End-to-end time.
    pub total_ms: u64,
}

/// Result of one download job.
#[derive(Debug, Clone, Serialize)]
pub struct DownloadResult {
    /// Post ID.
    pub post_id: String,
    /// Original input.
    pub source_url: String,
    /// Canonical page URL.
    pub page_url: String,
    /// Author.
    pub author: crate::model::Author,
    /// Caption.
    pub description: Option<String>,
    /// Video file, if any.
    pub video: Option<DownloadedFile>,
    /// Gallery photos in post order.
    pub photos: Vec<DownloadedFile>,
    /// Music file, if requested.
    pub audio: Option<DownloadedFile>,
    /// Gallery items that failed under partial mode.
    pub failures: Vec<PartialFailure>,
    /// Whether the result came from the content-addressed file cache.
    pub cache_hit: bool,
    /// Stage timings.
    pub timings: Timings,
}

/// Main public downloader facade.
#[derive(Clone)]
pub struct Ystt {
    config: YsttConfig,
    http: HttpClient,
    waf: WafSolver,
    cache: std::sync::Arc<UrlCache>,
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
        let http = HttpClient::new(config.http.clone())?;
        let waf = WafSolver::new(config.waf.clone());
        let cache = std::sync::Arc::new(UrlCache::new(config.cache_ttl));
        Ok(Self {
            config,
            http,
            waf,
            cache,
            waf_cookies: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
        })
    }

    /// Effective configuration.
    pub fn config(&self) -> &YsttConfig {
        &self.config
    }

    /// Remove stale file-cache manifests and unreferenced objects.
    pub async fn prune_file_cache(&self) -> Result<usize> {
        FileCache::new(
            self.file_cache_root(&self.config.output_dir),
            self.config.file_cache_ttl,
        )
        .prune()
        .await
    }

    /// Delete all file-cache manifests and objects.
    pub async fn clear_file_cache(&self) -> Result<()> {
        FileCache::new(
            self.file_cache_root(&self.config.output_dir),
            self.config.file_cache_ttl,
        )
        .clear()
        .await
    }

    /// Clear the in-memory signed-URL cache.
    pub async fn clear_url_cache(&self) {
        self.cache.clear().await;
    }

    /// Invalidates one signed-URL cache entry.
    pub async fn invalidate_url_cache(&self, post_id: &str, quality: QualityPolicy) {
        self.cache
            .invalidate(&format!("{post_id}:{}", quality.as_str()))
            .await;
    }

    /// Invalidate one file-cache manifest. Unreferenced objects are removed by
    /// the next [`Self::prune_file_cache`] call.
    pub async fn invalidate_file_cache(
        &self,
        post_id: &str,
        quality: QualityPolicy,
        include_audio: bool,
    ) -> Result<bool> {
        FileCache::new(
            self.file_cache_root(&self.config.output_dir),
            self.config.file_cache_ttl,
        )
        .invalidate(&file_cache_key(post_id, quality, include_audio))
        .await
    }

    /// Resolve metadata without downloading media.
    pub async fn resolve(&self, url: &str) -> Result<MediaInfo> {
        Ok(self
            .resolve_internal(url, &DownloadOptions::default())
            .await?
            .info)
    }

    /// Resolve with per-call overrides.
    pub async fn resolve_with(&self, url: &str, options: &DownloadOptions) -> Result<MediaInfo> {
        Ok(self.resolve_internal(url, options).await?.info)
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
        let started = Instant::now();
        let output_dir = options
            .output_dir
            .clone()
            .unwrap_or_else(|| self.config.output_dir.clone());
        let quality = options.quality.unwrap_or(self.config.quality);
        let include_audio = options.include_audio.unwrap_or(self.config.include_audio);
        let file_cache_enabled = options.file_cache.unwrap_or(self.config.file_cache);
        let allow_partial = options.allow_partial.unwrap_or(self.config.allow_partial);

        emit(
            &options,
            ProgressEvent::Resolving {
                url: url.to_owned(),
            },
        );

        if file_cache_enabled
            && let Some(result) = self
                .try_file_cache(url, &options, &output_dir, quality, include_audio)
                .await?
        {
            emit(
                &options,
                ProgressEvent::Completed {
                    total_ms: result.timings.total_ms,
                },
            );
            return Ok(result);
        }

        let mut resolved = self.resolve_internal(url, &options).await?;

        if resolved.info.is_age_restricted {
            return Err(YsttError::AgeRestricted);
        }
        if !resolved.info.has_media() {
            return Err(YsttError::NoMedia);
        }

        emit(
            &options,
            ProgressEvent::Resolved {
                post_id: resolved.info.post_id.clone(),
                media_count: resolved.info.video_url.is_some() as usize
                    + resolved.info.photo_urls.len()
                    + (include_audio && resolved.info.audio_url.is_some()) as usize,
            },
        );

        let mut media_elapsed = Duration::ZERO;
        let mut re_resolved = false;
        let (video, photos, audio, failures) = loop {
            let attempt_started = Instant::now();
            match self
                .transfer(
                    &resolved.info,
                    &output_dir,
                    &options,
                    resolved.cookie.as_deref(),
                )
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
                            self.cache.invalidate(&resolved.cache_key).await;
                        }
                        resolved = self.resolve_internal_mode(url, &options, false).await?;
                        re_resolved = true;
                        continue;
                    }
                    return Err(error);
                }
            }
        };

        if !allow_partial && !failures.is_empty() {
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

        if file_cache_enabled && failures.is_empty() {
            let key = file_cache_key(&resolved.info.post_id, quality, include_audio);
            if let Err(error) = self
                .store_file_cache(&key, &resolved.info, &video, &photos, &audio, &output_dir)
                .await
            {
                tracing::warn!(?error, "failed to store file cache");
            }
        }

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
        emit(
            &options,
            ProgressEvent::Completed {
                total_ms: result.timings.total_ms,
            },
        );
        Ok(result)
    }

    async fn try_file_cache(
        &self,
        url: &str,
        options: &DownloadOptions,
        output_dir: &Path,
        quality: QualityPolicy,
        include_audio: bool,
    ) -> Result<Option<DownloadResult>> {
        let started = Instant::now();
        let parsed = TikTokUrl::parse(url)?;
        let signal = options.signal.as_ref();
        let post_id = self.resolve_post_id(&parsed, signal).await?;
        let key = file_cache_key(&post_id, quality, include_audio);
        let cache = FileCache::new(self.file_cache_root(output_dir), self.config.file_cache_ttl);

        let Some(entry) = cache.get(&key).await else {
            return Ok(None);
        };

        let materialize_started = Instant::now();
        let video = match &entry.video {
            Some(cached) => match materialize_cached(&cache, cached, output_dir).await {
                Ok(file) => Some(file),
                Err(error) => {
                    tracing::warn!(?error, "failed to materialize cached video");
                    return Ok(None);
                }
            },
            None => None,
        };
        let mut photos = Vec::with_capacity(entry.photos.len());
        for cached in &entry.photos {
            match materialize_cached(&cache, cached, output_dir).await {
                Ok(file) => photos.push(file),
                Err(error) => {
                    tracing::warn!(?error, "failed to materialize cached photo");
                    return Ok(None);
                }
            }
        }
        let audio = match &entry.audio {
            Some(cached) => match materialize_cached(&cache, cached, output_dir).await {
                Ok(file) => Some(file),
                Err(error) => {
                    tracing::warn!(?error, "failed to materialize cached audio");
                    return Ok(None);
                }
            },
            None => None,
        };

        let elapsed = started.elapsed();
        let media_elapsed = materialize_started.elapsed();
        let mut info = entry.info;
        info.source_url = url.to_owned();
        emit(
            options,
            ProgressEvent::CacheHit {
                post_id: info.post_id.clone(),
            },
        );

        Ok(Some(DownloadResult {
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
        }))
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
        let cache = FileCache::new(self.file_cache_root(output_dir), self.config.file_cache_ttl);
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
            .file_cache_dir
            .clone()
            .unwrap_or_else(|| output_dir.join(".ystt-cache"))
    }

    async fn transfer(
        &self,
        info: &MediaInfo,
        output_dir: &Path,
        options: &DownloadOptions,
        cookie: Option<&str>,
    ) -> Result<(
        Option<DownloadedFile>,
        Vec<DownloadedFile>,
        Option<DownloadedFile>,
        Vec<PartialFailure>,
    )> {
        let parallel_ranges = options
            .parallel_ranges
            .unwrap_or(self.config.parallel_ranges);
        let max_connections = options
            .max_connections
            .unwrap_or(self.config.max_connections)
            .max(1);
        let photo_concurrency = options
            .photo_concurrency
            .unwrap_or(self.config.photo_concurrency)
            .max(1);
        let max_bytes = options.max_bytes.unwrap_or(self.config.max_bytes);
        let include_audio = options.include_audio.unwrap_or(self.config.include_audio);
        let reuse_existing = options.reuse_existing.unwrap_or(self.config.reuse_existing);
        let media_retries = options.media_retries.unwrap_or(self.config.media_retries);

        let context = MediaContext {
            client: &self.http,
            referer: &info.page_url,
            cookie,
            signal: options.signal.as_ref(),
            max_bytes,
            read_timeout: self.config.http.read_timeout,
            parallel_ranges,
            max_connections,
            first_chunk: self.config.range_first_chunk,
            min_chunk: self.config.range_min_chunk,
            range_threshold: self.config.range_threshold,
            reuse_existing,
            media_retries,
            progress: options.progress.clone(),
            adaptive_ranges: options
                .adaptive_ranges
                .unwrap_or(self.config.adaptive_ranges),
            range_start_connections: self.config.range_start_connections,
        };

        let base = base_filename(&info.author.id, &info.post_id);
        let output_dir = output_dir.to_path_buf();
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

    async fn resolve_internal(&self, url: &str, options: &DownloadOptions) -> Result<Resolved> {
        self.resolve_internal_mode(url, options, self.config.use_embed)
            .await
    }

    async fn resolve_internal_mode(
        &self,
        url: &str,
        options: &DownloadOptions,
        allow_embed: bool,
    ) -> Result<Resolved> {
        let started = Instant::now();
        let parsed = TikTokUrl::parse(url)?;
        let signal = options.signal.as_ref();

        check_cancelled(signal)?;

        let post_id = self.resolve_post_id(&parsed, signal).await?;

        let quality = options.quality.unwrap_or(self.config.quality);
        let cache_key = format!("{post_id}:{}", quality.as_str());

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

        let page_url = parsed.page_url(&post_id);
        let page_url = match &self.config.page_origin {
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
                    self.fetch_detail(&page_url, &post_id, signal, options.progress.as_ref())
                        .await?
                }
            }
        } else {
            match self
                .fetch_detail(&page_url, &post_id, signal, options.progress.as_ref())
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
        };

        if !info.has_media() && !info.is_age_restricted {
            return Err(YsttError::NoMedia);
        }

        if info.has_media() {
            self.cache.put(cache_key.clone(), info.clone()).await;
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
                self.config.max_page_bytes,
                self.config.http.read_timeout,
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
                self.config.max_page_bytes,
                self.config.http.read_timeout,
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
        progress: Option<&ProgressReporter>,
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
                    self.config.max_page_bytes,
                    self.config.http.read_timeout,
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
                            progress.report(ProgressEvent::WafChallenge { attempt });
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
                            progress.report(ProgressEvent::WafSolved {
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

/// Builder for [`Ystt`].
#[derive(Debug, Clone, Default)]
pub struct YsttBuilder {
    config: YsttConfig,
}

impl YsttBuilder {
    /// New builder with defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Default output directory.
    pub fn output_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.config.output_dir = path.into();
        self
    }

    /// Default quality policy.
    pub fn quality(mut self, quality: QualityPolicy) -> Self {
        self.config.quality = quality;
        self
    }

    /// Download music tracks.
    ///
    /// Audio uses the same pipeline and ALPN Auto transport as video; H1-only
    /// music hosts fall back to HTTP/1.1 automatically.
    pub fn include_audio(mut self, include: bool) -> Self {
        self.config.include_audio = include;
        self
    }

    /// Enable range-first parallel downloads.
    pub fn parallel_ranges(mut self, enabled: bool) -> Self {
        self.config.parallel_ranges = enabled;
        self
    }

    /// Maximum range connections.
    pub fn max_connections(mut self, connections: usize) -> Self {
        self.config.max_connections = connections.max(1);
        self
    }

    /// Maximum photo concurrency.
    pub fn photo_concurrency(mut self, concurrency: usize) -> Self {
        self.config.photo_concurrency = concurrency.max(1);
        self
    }

    /// Maximum media size.
    pub fn max_bytes(mut self, max: u64) -> Self {
        self.config.max_bytes = max;
        self
    }

    /// Maximum page/JSON size.
    pub fn max_page_bytes(mut self, max: u64) -> Self {
        self.config.max_page_bytes = max;
        self
    }

    /// Browser fingerprint profile.
    pub fn browser(mut self, browser: ystt_net::BrowserProfile) -> Self {
        self.config.http.browser = browser;
        self
    }

    /// HTTP transport policy (`Auto`, `Http2Only`, `Http1Only`).
    pub fn transport(mut self, transport: ystt_net::TransportPolicy) -> Self {
        self.config.http.transport = transport;
        self
    }

    /// Total request timeout.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.config.http.timeout = timeout;
        self
    }

    /// Connection timeout.
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.config.http.connect_timeout = timeout;
        self
    }

    /// Body idle timeout.
    pub fn read_timeout(mut self, timeout: Duration) -> Self {
        self.config.http.read_timeout = timeout;
        self
    }

    /// HTTP retry count.
    pub fn retries(mut self, retries: u32) -> Self {
        self.config.http.retries = retries;
        self
    }

    /// Page concurrency per host.
    pub fn per_host_concurrency(mut self, concurrency: usize) -> Self {
        self.config.http.per_host_concurrency = concurrency.max(1);
        self
    }

    /// Minimum spacing between page requests.
    pub fn min_request_interval(mut self, interval: Duration) -> Self {
        self.config.http.min_request_interval = interval;
        self
    }

    /// Override the emulated user-agent.
    pub fn user_agent(mut self, user_agent: impl Into<String>) -> Self {
        self.config.http.user_agent = Some(user_agent.into());
        self
    }

    /// Disable environment/system proxies.
    pub fn no_proxy(mut self, disabled: bool) -> Self {
        self.config.http.no_proxy = disabled;
        self
    }

    /// Enable or disable the lightweight embed resolver.
    pub fn use_embed(mut self, enabled: bool) -> Self {
        self.config.use_embed = enabled;
        self
    }

    /// WAF solver thread count.
    pub fn waf_threads(mut self, threads: usize) -> Self {
        self.config.waf.threads = Some(threads.max(1));
        self
    }

    /// WAF solver iteration cap.
    pub fn waf_max_iterations(mut self, max: u64) -> Self {
        self.config.waf.max_iterations = max;
        self
    }

    /// Signed-URL cache TTL.
    pub fn cache_ttl(mut self, ttl: Duration) -> Self {
        self.config.cache_ttl = ttl;
        self
    }

    /// Reuse existing output files.
    pub fn reuse_existing(mut self, reuse: bool) -> Self {
        self.config.reuse_existing = reuse;
        self
    }

    /// Enable or disable the content-addressed file cache.
    pub fn file_cache(mut self, enabled: bool) -> Self {
        self.config.file_cache = enabled;
        self
    }

    /// Override the file-cache root.
    pub fn file_cache_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.config.file_cache_dir = Some(path.into());
        self
    }

    /// File-cache TTL.
    pub fn file_cache_ttl(mut self, ttl: Duration) -> Self {
        self.config.file_cache_ttl = ttl;
        self
    }

    /// Advanced: override the page origin (local integration tests/mirrors).
    pub fn page_origin(mut self, origin: impl Into<String>) -> Self {
        self.config.page_origin = Some(origin.into().trim_end_matches('/').to_owned());
        self
    }

    /// Retry budget for each media item.
    pub fn media_retries(mut self, retries: u32) -> Self {
        self.config.media_retries = retries;
        self
    }

    /// Allow partial gallery results.
    pub fn allow_partial(mut self, allow: bool) -> Self {
        self.config.allow_partial = allow;
        self
    }

    /// Enable adaptive range concurrency.
    pub fn adaptive_ranges(mut self, enabled: bool) -> Self {
        self.config.adaptive_ranges = enabled;
        self
    }

    /// Initial range connections when adaptive mode is enabled.
    pub fn range_start_connections(mut self, connections: usize) -> Self {
        self.config.range_start_connections = connections.max(1);
        self
    }

    /// First range size for videos.
    pub fn range_first_chunk(mut self, bytes: u64) -> Self {
        self.config.range_first_chunk = bytes.max(1);
        self
    }

    /// Minimum follow-up range size.
    pub fn range_min_chunk(mut self, bytes: u64) -> Self {
        self.config.range_min_chunk = bytes.max(1);
        self
    }

    /// Files at or below this size use one normal GET instead of ranges.
    pub fn range_threshold(mut self, bytes: u64) -> Self {
        self.config.range_threshold = bytes;
        self
    }

    /// Build the downloader.
    pub fn build(self) -> Result<Ystt> {
        Ystt::new(self.config)
    }
}

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

#[derive(Default)]
struct CookieJar {
    values: Vec<(String, String)>,
}

impl CookieJar {
    fn absorb(&mut self, set_cookie: &str) {
        let pair = set_cookie.split(';').next().unwrap_or_default();
        self.set(pair);
    }

    fn set(&mut self, pair: &str) {
        let Some((name, value)) = pair.split_once('=') else {
            return;
        };
        let name = name.trim();
        let value = value.trim();
        if name.is_empty() || value.is_empty() {
            return;
        }
        if let Some(existing) = self
            .values
            .iter_mut()
            .find(|(existing, _)| existing == name)
        {
            existing.1 = value.to_owned();
        } else {
            self.values.push((name.to_owned(), value.to_owned()));
        }
    }

    fn header(&self) -> Option<String> {
        if self.values.is_empty() {
            return None;
        }
        Some(
            self.values
                .iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect::<Vec<_>>()
                .join("; "),
        )
    }
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

fn emit(options: &DownloadOptions, event: ProgressEvent) {
    if let Some(progress) = &options.progress {
        progress.report(event);
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
    fn cookie_jar_parses_and_replaces() {
        let mut jar = CookieJar::default();
        jar.absorb("ttwid=abc; Path=/; HttpOnly");
        jar.absorb("waf_cookie=xyz; Path=/");
        assert_eq!(jar.header().as_deref(), Some("ttwid=abc; waf_cookie=xyz"));
        jar.set("waf_cookie=second");
        assert_eq!(
            jar.header().as_deref(),
            Some("ttwid=abc; waf_cookie=second")
        );
    }

    #[test]
    fn cookie_jar_ignores_invalid_pairs() {
        let mut jar = CookieJar::default();
        jar.absorb("novalue; Path=/");
        jar.absorb("=empty");
        assert!(jar.header().is_none());
    }

    #[test]
    fn millis_are_saturated() {
        assert_eq!(millis(Duration::from_millis(42)), 42);
    }
}
