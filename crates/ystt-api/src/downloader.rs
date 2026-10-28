//! In-process download service and conversion to API descriptors.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use futures_util::{StreamExt, stream};
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;
use ystt::{
    Author, DownloadOptions, Http2Tuning, InMemoryFile, InMemoryPost, MediaKind, Provider,
    QualityPolicy, SoundCloudArtwork, SoundCloudAudio, SoundCloudEntity, Timings, Ystt,
    is_soundcloud_url, normalize_soundcloud_url,
};

use crate::config::ApiConfig;
use crate::dto::{
    BatchItem, BatchResponse, DownloadOptionsInput, DownloadResponse, MediaDescriptor,
    PlaylistDescriptor, PlaylistFailure, PlaylistTrackDescriptor, ResolveResponse,
};
use crate::error::ApiError;
use crate::store::{MediaStore, StoredMedia};

/// Shared in-process download service.
struct CachedResult {
    response: DownloadResponse,
    created: Instant,
    expires: Instant,
}

/// Shared in-process download service.
#[derive(Clone)]
pub struct Downloader {
    ystt: Ystt,
    permits: Arc<Semaphore>,
    store: MediaStore,
    config: Arc<ApiConfig>,
    results: Arc<RwLock<HashMap<String, CachedResult>>>,
}

impl Downloader {
    /// Build the downloader once and reuse its HTTP connection pool.
    pub fn new(config: Arc<ApiConfig>) -> Result<Self, ApiError> {
        let ystt = Ystt::builder()
            .quality(QualityPolicy::Balanced)
            .max_bytes(config.max_bytes)
            .file_cache(false)
            .metadata_cache(false)
            .parallel_ranges(true)
            .max_connections(6)
            .range_threshold(8 * 1024 * 1024)
            .photo_concurrency(8)
            .pool_max_idle_per_host(8)
            .per_host_concurrency(4)
            .timeout(Duration::from_secs(120))
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(120))
            .retries(2)
            .http2_tuning(Http2Tuning {
                connection_window: Some(64 * 1024 * 1024),
                keep_alive_interval: Some(Duration::from_secs(30)),
                keep_alive_timeout: Some(Duration::from_secs(5)),
                keep_alive_while_idle: Some(true),
            })
            .build()?;
        let store = MediaStore::new(config.media_ttl, config.store_bytes);
        Ok(Self {
            ystt,
            permits: Arc::new(Semaphore::new(config.workers.max(1))),
            store,
            config,
            results: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// Shared bounded media store.
    pub fn store(&self) -> &MediaStore {
        &self.store
    }

    /// Effective API configuration.
    pub fn config(&self) -> &ApiConfig {
        &self.config
    }

    /// Resolve one URL without downloading media bytes.
    pub async fn resolve(
        &self,
        url: &str,
        input: &DownloadOptionsInput,
    ) -> Result<ResolveResponse, ApiError> {
        let options = input.to_options(&self.config)?;
        if is_soundcloud_url(url) {
            let entity = self
                .ystt
                .resolve_soundcloud_with(url, &options)
                .await
                .map_err(ApiError::from)?;
            return Ok(ResolveResponse {
                provider: Provider::SoundCloud,
                info: None,
                soundcloud: Some(entity),
            });
        }
        let info = self
            .ystt
            .resolve_with(url, &options)
            .await
            .map_err(ApiError::from)?;
        Ok(ResolveResponse {
            provider: Provider::TikTok,
            info: Some(info),
            soundcloud: None,
        })
    }

    /// Download one post into memory and return authenticated descriptors.
    pub async fn download(
        &self,
        url: &str,
        input: &DownloadOptionsInput,
    ) -> Result<DownloadResponse, ApiError> {
        let key = cache_key(url, input);
        if let Some(response) = self.cached(&key) {
            return Ok(response);
        }
        let _permit = self.permits.acquire().await.map_err(|_| {
            ApiError::new(
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "queue_closed",
                "download queue closed",
            )
        })?;
        let options = input.to_options(&self.config)?;
        let response = if is_soundcloud_url(url) {
            self.download_soundcloud(url, options).await?
        } else {
            let post = self
                .ystt
                .download_bytes_with(url, options)
                .await
                .map_err(ApiError::from)?;
            self.response(post)?
        };
        self.cache(key, response.clone());
        Ok(response)
    }

    async fn download_soundcloud(
        &self,
        url: &str,
        options: DownloadOptions,
    ) -> Result<DownloadResponse, ApiError> {
        let started = Instant::now();
        let entity = self
            .ystt
            .resolve_soundcloud_with(url, &options)
            .await
            .map_err(ApiError::from)?;
        match entity {
            SoundCloudEntity::Track(track) => {
                let audio = self
                    .ystt
                    .download_soundcloud_resolved_track(&track, &options)
                    .await
                    .map_err(ApiError::from)?;
                let descriptor = self.describe_audio(&audio);
                let artwork_descriptor = audio
                    .artwork
                    .as_ref()
                    .map(|artwork| self.describe_artwork(artwork));
                let artwork = audio.artwork_url().to_owned();
                Ok(DownloadResponse {
                    provider: Provider::SoundCloud,
                    post_id: track.id.clone(),
                    source_url: url.to_owned(),
                    page_url: track.permalink_url.clone(),
                    author: Author {
                        id: if track.artist.is_empty() {
                            track.id.clone()
                        } else {
                            track.artist.clone()
                        },
                        nickname: Some(track.artist.clone()),
                    },
                    description: track.description.clone(),
                    cover_url: Some(artwork.clone()),
                    title: Some(track.title.clone()),
                    artwork_url: Some(artwork),
                    artwork: artwork_descriptor,
                    video: None,
                    photos: Vec::new(),
                    audio: Some(descriptor),
                    playlist: None,
                    failures: Vec::new(),
                    timings: Timings {
                        total_ms: started.elapsed().as_millis() as u64,
                        media_ms: started.elapsed().as_millis() as u64,
                        ..Timings::default()
                    },
                    cache_hit: false,
                })
            }
            SoundCloudEntity::Playlist(playlist) => {
                if playlist.tracks.len() > self.config.max_playlist_tracks {
                    return Err(ApiError::new(
                        axum::http::StatusCode::BAD_REQUEST,
                        "playlist_too_large",
                        format!(
                            "playlist has {} tracks; API limit is {}",
                            playlist.tracks.len(),
                            self.config.max_playlist_tracks
                        ),
                    ));
                }
                let total_tracks = playlist.tracks.len();
                let downloaded = self
                    .ystt
                    .download_soundcloud_resolved_playlist(
                        &playlist,
                        &options,
                        (self.config.batch_concurrency * 2).clamp(1, 4),
                    )
                    .await
                    .map_err(ApiError::from)?;
                let tracks: Vec<PlaylistTrackDescriptor> = downloaded
                    .tracks
                    .iter()
                    .map(|audio| PlaylistTrackDescriptor {
                        track_id: audio.track.id.clone(),
                        title: audio.track.title.clone(),
                        artist: audio.track.artist.clone(),
                        page_url: audio.track.permalink_url.clone(),
                        artwork_url: audio.track.artwork_url.clone(),
                        duration_sec: audio.duration_sec(),
                        audio: self.describe_audio(audio),
                    })
                    .collect();
                let failures: Vec<PlaylistFailure> = downloaded
                    .failures
                    .iter()
                    .map(|failure| PlaylistFailure {
                        track_id: failure.track_id.clone(),
                        title: failure.title.clone(),
                        url: failure.url.clone(),
                        code: failure.error.code.to_owned(),
                        message: failure.error.message.clone(),
                    })
                    .collect();
                let artwork = downloaded.playlist.artwork().to_owned();
                let artwork_descriptor = downloaded
                    .tracks
                    .iter()
                    .find_map(|audio| audio.artwork.as_ref())
                    .map(|artwork| self.describe_artwork(artwork));
                Ok(DownloadResponse {
                    provider: Provider::SoundCloud,
                    post_id: downloaded.playlist.id.clone(),
                    source_url: url.to_owned(),
                    page_url: downloaded.playlist.permalink_url.clone(),
                    author: Author {
                        id: if downloaded.playlist.artist.is_empty() {
                            downloaded.playlist.id.clone()
                        } else {
                            downloaded.playlist.artist.clone()
                        },
                        nickname: Some(downloaded.playlist.artist.clone()),
                    },
                    description: downloaded.playlist.description.clone(),
                    cover_url: Some(artwork.clone()),
                    title: Some(downloaded.playlist.title.clone()),
                    artwork_url: Some(artwork),
                    artwork: artwork_descriptor.clone(),
                    video: None,
                    photos: Vec::new(),
                    audio: None,
                    playlist: Some(PlaylistDescriptor {
                        playlist_id: downloaded.playlist.id,
                        title: downloaded.playlist.title,
                        artist: downloaded.playlist.artist,
                        page_url: downloaded.playlist.permalink_url,
                        artwork_url: downloaded.playlist.artwork_url,
                        artwork: artwork_descriptor,
                        track_count: total_tracks,
                        tracks,
                        failures,
                    }),
                    failures: Vec::new(),
                    timings: Timings {
                        total_ms: started.elapsed().as_millis() as u64,
                        media_ms: started.elapsed().as_millis() as u64,
                        ..Timings::default()
                    },
                    cache_hit: false,
                })
            }
        }
    }

    fn describe_audio(&self, audio: &SoundCloudAudio) -> MediaDescriptor {
        let sha256 = sha256_hex(&audio.bytes);
        let token = self.store.insert(StoredMedia {
            bytes: audio.bytes.clone(),
            mime: audio.mime.clone(),
            ext: audio.ext.clone(),
            kind: MediaKind::Audio,
            sha256: sha256.clone(),
            source_url: audio.source_url.clone(),
            created: Instant::now(),
            expires: Instant::now(),
        });
        MediaDescriptor {
            url: self.config.media_url(&token),
            token,
            kind: MediaKind::Audio,
            mime: audio.mime.clone(),
            ext: audio.ext.clone(),
            size: audio.bytes.len() as u64,
            sha256,
            source_url: audio.source_url.clone(),
            width: None,
            height: None,
            duration_sec: audio.duration_sec(),
            title: Some(audio.track.title.clone()),
            artist: Some(audio.track.artist.clone()),
        }
    }

    fn cached(&self, key: &str) -> Option<DownloadResponse> {
        let now = Instant::now();
        let mut results = self.results.write().expect("result cache lock");
        let entry = results.get(key)?;
        if entry.expires <= now || !response_is_live(&entry.response, &self.store) {
            results.remove(key);
            return None;
        }
        let mut response = entry.response.clone();
        response.cache_hit = true;
        Some(response)
    }

    fn cache(&self, key: String, response: DownloadResponse) {
        let now = Instant::now();
        let mut results = self.results.write().expect("result cache lock");
        results.retain(|_, entry| entry.expires > now);
        if results.len() >= 512 {
            let oldest = results
                .iter()
                .min_by_key(|(_, entry)| entry.created)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                results.remove(&oldest);
            }
        }
        results.insert(
            key,
            CachedResult {
                response,
                created: now,
                expires: now + self.config.media_ttl,
            },
        );
    }

    /// Number of live cached download results.
    pub fn result_cache_entries(&self) -> usize {
        self.results.read().expect("result cache lock").len()
    }

    /// Download multiple URLs with bounded concurrency, preserving order.
    pub async fn batch(
        &self,
        urls: Vec<String>,
        concurrency: Option<usize>,
        item_timeout: Option<Duration>,
        item_retries: Option<u32>,
        input: &DownloadOptionsInput,
    ) -> Result<BatchResponse, ApiError> {
        if urls.is_empty() {
            return Err(ApiError::new(
                axum::http::StatusCode::BAD_REQUEST,
                "invalid_request",
                "urls must not be empty",
            ));
        }
        if urls.len() > self.config.max_batch {
            return Err(ApiError::new(
                axum::http::StatusCode::BAD_REQUEST,
                "batch_too_large",
                format!("at most {} URLs are allowed", self.config.max_batch),
            ));
        }
        let started = Instant::now();
        let concurrency = concurrency
            .unwrap_or(self.config.batch_concurrency)
            .clamp(1, self.config.batch_concurrency);
        let total = urls.len();
        let mut results: Vec<Option<BatchItem>> = vec![None; total];
        let mut items = stream::iter(urls.into_iter().enumerate().map(|(index, url)| {
            let downloader = self.clone();
            let mut input = input.clone();
            if let Some(retries) = item_retries {
                input.media_retries = Some(retries);
            }
            async move {
                let future = downloader.download(&url, &input);
                let result = match item_timeout {
                    Some(timeout) => match tokio::time::timeout(timeout, future).await {
                        Ok(result) => result,
                        Err(_) => Err(ApiError::new(
                            axum::http::StatusCode::GATEWAY_TIMEOUT,
                            "timeout",
                            "batch item timed out",
                        )),
                    },
                    None => future.await,
                };
                let item = match result {
                    Ok(result) => BatchItem {
                        index,
                        url,
                        result: Some(result),
                        error: None,
                    },
                    Err(error) => BatchItem {
                        index,
                        url,
                        result: None,
                        error: Some(error.body.clone()),
                    },
                };
                (index, item)
            }
        }))
        .buffer_unordered(concurrency);
        while let Some((index, item)) = items.next().await {
            results[index] = Some(item);
        }
        let items: Vec<BatchItem> = results.into_iter().flatten().collect();
        let successes = items.iter().filter(|item| item.result.is_some()).count();
        let failures = items.len().saturating_sub(successes);
        Ok(BatchResponse {
            items,
            successes,
            failures,
            total_ms: started.elapsed().as_millis() as u64,
        })
    }

    fn response(&self, post: InMemoryPost) -> Result<DownloadResponse, ApiError> {
        let video = post.video.map(|file| self.describe(file));
        let photos: Vec<MediaDescriptor> = post
            .photos
            .into_iter()
            .map(|file| self.describe(file))
            .collect();
        let audio = post.audio.map(|file| self.describe(file));
        if video.is_none() && photos.is_empty() && audio.is_none() {
            return Err(ApiError::from(ystt::YsttError::NoMedia));
        }
        Ok(DownloadResponse {
            provider: Provider::TikTok,
            post_id: post.post_id,
            source_url: post.source_url,
            page_url: post.page_url,
            author: post.author,
            description: post.description,
            cover_url: post.cover_url.clone(),
            title: None,
            artwork_url: None,
            artwork: None,
            video,
            photos,
            audio,
            playlist: None,
            failures: post.failures,
            timings: post.timings,
            cache_hit: false,
        })
    }

    fn describe_artwork(&self, artwork: &SoundCloudArtwork) -> MediaDescriptor {
        let sha256 = sha256_hex(&artwork.bytes);
        let ext = match artwork.mime.as_str() {
            "image/png" => "png",
            "image/webp" => "webp",
            _ => "jpg",
        };
        let token = self.store.insert(StoredMedia {
            bytes: artwork.bytes.clone(),
            mime: artwork.mime.clone(),
            ext: ext.into(),
            kind: MediaKind::Photo,
            sha256: sha256.clone(),
            source_url: artwork.url.clone(),
            created: Instant::now(),
            expires: Instant::now(),
        });
        MediaDescriptor {
            url: self.config.media_url(&token),
            token,
            kind: MediaKind::Photo,
            mime: artwork.mime.clone(),
            ext: ext.into(),
            size: artwork.bytes.len() as u64,
            sha256,
            source_url: artwork.url.clone(),
            width: Some(300),
            height: Some(300),
            duration_sec: None,
            title: None,
            artist: None,
        }
    }

    fn describe(&self, file: InMemoryFile) -> MediaDescriptor {
        let sha256 = sha256_hex(&file.bytes);
        let token = self.store.insert(StoredMedia {
            bytes: file.bytes,
            mime: file.mime.clone(),
            ext: file.ext.clone(),
            kind: file.kind,
            sha256: sha256.clone(),
            source_url: file.source_url.clone(),
            created: Instant::now(),
            expires: Instant::now(),
        });
        MediaDescriptor {
            url: self.config.media_url(&token),
            token,
            kind: file.kind,
            mime: file.mime,
            ext: file.ext,
            size: file.size,
            sha256,
            source_url: file.source_url,
            width: file.width,
            height: file.height,
            duration_sec: file.duration_sec,
            title: None,
            artist: None,
        }
    }
}

fn cache_key(url: &str, input: &DownloadOptionsInput) -> String {
    let normalized = if is_soundcloud_url(url) {
        normalize_soundcloud_url(url)
            .map(|url| format!("soundcloud:{url}"))
            .unwrap_or_else(|_| format!("soundcloud:{}", url.trim()))
    } else {
        let key = ystt::TikTokUrl::parse(url)
            .ok()
            .and_then(|parsed| parsed.post_id().map(|id| format!("post:{id}")))
            .unwrap_or_else(|| url.trim().to_owned());
        format!("tiktok:{key}")
    };
    format!("{normalized}|{input:?}")
}

fn response_is_live(response: &DownloadResponse, store: &MediaStore) -> bool {
    response
        .video
        .iter()
        .chain(response.photos.iter())
        .chain(response.audio.iter())
        .chain(response.artwork.iter())
        .chain(
            response
                .playlist
                .iter()
                .flat_map(|playlist| playlist.artwork.iter()),
        )
        .chain(
            response
                .playlist
                .iter()
                .flat_map(|playlist| playlist.tracks.iter().map(|track| &track.audio)),
        )
        .all(|descriptor| store.get(&descriptor.token).is_some())
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn cache_keys_are_provider_namespaced_and_share_query_is_ignored() {
        let input = DownloadOptionsInput::default();
        let track = cache_key("https://soundcloud.com/u/t?si=x&utm_source=y", &input);
        let again = cache_key("https://soundcloud.com/u/t", &input);
        assert_eq!(track, again);
        assert!(track.starts_with("soundcloud:https://soundcloud.com/u/t"));
        assert!(
            cache_key(
                "https://www.tiktok.com/@u/video/7687283866586909984?x=1",
                &input
            )
            .starts_with("tiktok:post:7687283866586909984")
        );
    }
}
