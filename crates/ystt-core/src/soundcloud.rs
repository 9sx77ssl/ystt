//! Public SoundCloud resolver and native HLS audio downloader.
//!
//! SoundCloud does not require an account for public tracks, but its public web
//! player still needs a short-lived `client_id`. This module discovers that
//! value from the public SoundCloud page, caches it, resolves tracks/playlists
//! through the public `api-v2` endpoint, and assembles audio natively.
//!
//! There is intentionally no yt-dlp, ffmpeg, browser automation, cookie jar or
//! account login path. Progressive MP3, HLS MP3 and fragmented-MP4 AAC are
//! supported directly.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use futures_util::{StreamExt, stream};
use http::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderValue, LOCATION, REFERER};
use regex::Regex;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use tracing::debug;
use url::Url;
use ystt_net::{HttpClient, NetError, RequestOptions};

use crate::config::QualityPolicy;
use crate::error::{Result, YsttError};

const API_ORIGIN: &str = "https://api-v2.soundcloud.com";
const HOME: &str = "https://soundcloud.com/";
const DEFAULT_ARTWORK: &str = "https://i1.sndcdn.com/artworks-000000000000-000000-t300x300.jpg";

/// One SoundCloud transcoding exposed by the public API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SoundCloudTranscoding {
    /// SoundCloud preset such as `aac_160k` or `mp3_1_0`.
    pub preset: Option<String>,
    /// API endpoint that returns the final signed stream URL.
    pub url: String,
    /// `progressive` or `hls`.
    pub protocol: Option<String>,
    /// Declared MIME type.
    pub mime_type: Option<String>,
    /// Whether the public API marks this stream as snipped.
    pub snipped: bool,
}

/// Downloaded square artwork for a SoundCloud track.
#[derive(Debug, Clone)]
pub struct SoundCloudArtwork {
    /// JPEG payload suitable for Telegram thumbnails.
    pub bytes: Bytes,
    /// Detected MIME type.
    pub mime: String,
    /// Normalized source URL.
    pub url: String,
}

/// A public SoundCloud track.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SoundCloudTrack {
    /// Numeric track ID.
    pub id: String,
    /// Track title.
    pub title: String,
    /// Uploader display name.
    pub artist: String,
    /// Uploader numeric ID, when known.
    pub artist_id: Option<String>,
    /// Duration in milliseconds, when known.
    pub duration_ms: Option<u64>,
    /// Canonical track page.
    pub permalink_url: String,
    /// Normalized square artwork URL.
    pub artwork_url: Option<String>,
    /// SoundCloud policy (`MONETIZE`, `BLOCK`, `SNIP`, ...).
    pub policy: Option<String>,
    /// Description, when present.
    pub description: Option<String>,
    /// Genre, when present.
    pub genre: Option<String>,
    /// Public stream candidates.
    pub transcodings: Vec<SoundCloudTranscoding>,
    /// Per-track stream authorization. Never serialized as public metadata.
    #[serde(skip)]
    track_authorization: Option<String>,
}

impl SoundCloudTrack {
    /// Duration rounded to seconds.
    pub fn duration_sec(&self) -> Option<u32> {
        self.duration_ms
            .map(|milliseconds| (milliseconds / 1000).min(u64::from(u32::MAX)) as u32)
    }

    /// Best available artwork, always safe as a Telegram URL when present.
    pub fn artwork(&self) -> &str {
        self.artwork_url.as_deref().unwrap_or(DEFAULT_ARTWORK)
    }
}

/// A public SoundCloud playlist/set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SoundCloudPlaylist {
    /// Playlist ID.
    pub id: String,
    /// Playlist title.
    pub title: String,
    /// Uploader/curator display name.
    pub artist: String,
    /// Canonical playlist page.
    pub permalink_url: String,
    /// Playlist description, when present.
    pub description: Option<String>,
    /// Duration in milliseconds, when known.
    pub duration_ms: Option<u64>,
    /// Playlist artwork, falling back to the first track's artwork.
    pub artwork_url: Option<String>,
    /// Tracks in playlist order.
    pub tracks: Vec<SoundCloudTrack>,
}

impl SoundCloudPlaylist {
    /// Duration rounded to seconds.
    pub fn duration_sec(&self) -> Option<u32> {
        self.duration_ms
            .map(|milliseconds| (milliseconds / 1000).min(u64::from(u32::MAX)) as u32)
    }

    /// Playlist artwork with the first track as a fallback.
    pub fn artwork(&self) -> &str {
        self.artwork_url
            .as_deref()
            .or_else(|| {
                self.tracks
                    .first()
                    .and_then(|track| track.artwork_url.as_deref())
            })
            .unwrap_or(DEFAULT_ARTWORK)
    }
}

/// Resolved SoundCloud entity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum SoundCloudEntity {
    /// A single track.
    Track(SoundCloudTrack),
    /// A playlist with its tracks.
    Playlist(SoundCloudPlaylist),
}

impl SoundCloudEntity {
    /// Stable provider name.
    pub fn provider(&self) -> &'static str {
        "soundcloud"
    }

    /// Canonical page URL.
    pub fn permalink_url(&self) -> &str {
        match self {
            Self::Track(track) => &track.permalink_url,
            Self::Playlist(playlist) => &playlist.permalink_url,
        }
    }

    /// Artwork URL.
    pub fn artwork_url(&self) -> &str {
        match self {
            Self::Track(track) => track.artwork(),
            Self::Playlist(playlist) => playlist.artwork(),
        }
    }
}

/// One fully assembled track held in memory.
#[derive(Debug, Clone)]
pub struct SoundCloudAudio {
    /// Track metadata.
    pub track: SoundCloudTrack,
    /// Complete audio payload.
    pub bytes: Bytes,
    /// MIME type.
    pub mime: String,
    /// Extension without a dot (`m4a`, `mp3`, `ogg`).
    pub ext: String,
    /// Final signed CDN URL used for transfer.
    pub source_url: String,
    /// Preset selected for transfer.
    pub preset: Option<String>,
    /// Downloaded artwork, if available.
    pub artwork: Option<SoundCloudArtwork>,
}

impl SoundCloudAudio {
    /// Artwork URL for thumbnails and metadata.
    pub fn artwork_url(&self) -> &str {
        self.track.artwork()
    }

    /// Duration rounded to seconds.
    pub fn duration_sec(&self) -> Option<u32> {
        self.track.duration_sec()
    }
}

/// One failed playlist track.
#[derive(Debug, Clone, Serialize)]
pub struct SoundCloudTrackFailure {
    /// Track ID.
    pub track_id: String,
    /// Track title.
    pub title: String,
    /// Track page.
    pub url: String,
    /// Stable error payload.
    pub error: crate::error::ErrorInfo,
}

/// All successfully downloaded tracks of a playlist, preserving order.
#[derive(Debug, Clone)]
pub struct SoundCloudPlaylistAudio {
    /// Playlist metadata.
    pub playlist: SoundCloudPlaylist,
    /// Downloaded tracks in playlist order.
    pub tracks: Vec<SoundCloudAudio>,
    /// Per-track failures when partial mode is enabled.
    pub failures: Vec<SoundCloudTrackFailure>,
}

/// Cheap-to-clone public SoundCloud client.
#[derive(Clone)]
pub struct SoundCloudClient {
    http: HttpClient,
    client_id: Arc<Mutex<Option<CachedClientId>>>,
    resolved: Arc<Mutex<HashMap<String, CachedEntity>>>,
    page_limit: u64,
    read_timeout: Duration,
}

#[derive(Debug, Clone)]
struct CachedClientId {
    id: String,
}

#[derive(Clone)]
struct CachedEntity {
    entity: SoundCloudEntity,
    inserted: Instant,
}

impl std::fmt::Debug for SoundCloudClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SoundCloudClient")
            .field("page_limit", &self.page_limit)
            .finish_non_exhaustive()
    }
}

impl SoundCloudClient {
    /// Create a SoundCloud client using the shared HTTP pool.
    pub fn new(http: HttpClient, page_limit: u64, read_timeout: Duration) -> Self {
        Self {
            http,
            client_id: Arc::new(Mutex::new(None)),
            resolved: Arc::new(Mutex::new(HashMap::new())),
            page_limit,
            read_timeout,
        }
    }

    /// Resolve a public track or playlist without downloading media.
    pub async fn resolve(&self, input: &str) -> Result<SoundCloudEntity> {
        self.resolve_with_signal(input, None).await
    }

    /// Resolve with cancellation support.
    pub async fn resolve_with_signal(
        &self,
        input: &str,
        signal: Option<&CancellationToken>,
    ) -> Result<SoundCloudEntity> {
        let input = self.expand_short_url(input, signal).await?;
        let input = normalize_url(&input)?;
        if let Some(entity) = self.cached_resolved(&input) {
            return Ok(entity);
        }
        let mut refreshed = false;
        let entity = loop {
            let client_id = self.client_id(signal).await?;
            let endpoint = resolve_endpoint(&input, &client_id)?;
            match self.get_json::<RawEntity>(&endpoint, signal).await {
                Ok(raw) => {
                    let entity = raw.into_entity()?;
                    break match entity {
                        SoundCloudEntity::Playlist(playlist) => SoundCloudEntity::Playlist(
                            self.hydrate_playlist(playlist, signal).await?,
                        ),
                        track => track,
                    };
                }
                Err(YsttError::Network(NetError::Status { status, .. }))
                    if matches!(status, 401 | 403) && !refreshed =>
                {
                    refreshed = true;
                    self.invalidate_client_id();
                    debug!(status, "stale SoundCloud client_id; refreshing once");
                }
                Err(error) => return Err(error),
            }
        };
        self.store_resolved(&input, &entity);
        Ok(entity)
    }

    /// Follow `on.soundcloud.com` share redirects without a browser or cookies.
    async fn expand_short_url(
        &self,
        input: &str,
        signal: Option<&CancellationToken>,
    ) -> Result<String> {
        let parsed = Url::parse(input.trim())
            .map_err(|error| YsttError::InvalidUrl(format!("{error}: {input}")))?;
        let short = parsed
            .host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case("on.soundcloud.com"));
        if !short {
            return Ok(input.trim().to_owned());
        }
        let mut current = parsed;
        for _ in 0..6 {
            let mut options = RequestOptions::short_link().with_retries(1);
            if let Some(signal) = signal {
                options = options.with_signal(signal.clone());
            }
            let response = self
                .http
                .get(current.as_str(), self.json_headers(), options)
                .await?;
            let status = response.status_u16();
            if (300..400).contains(&status)
                && let Some(location) = response
                    .headers()
                    .get(LOCATION)
                    .and_then(|value| value.to_str().ok())
            {
                current = current
                    .join(location)
                    .map_err(|error| YsttError::Parse(format!("invalid short URL: {error}")))?;
                continue;
            }
            if response.is_success() {
                return Ok(current.to_string());
            }
            return Err(YsttError::Unavailable(format!(
                "SoundCloud short link unavailable (HTTP {status})"
            )));
        }
        Err(YsttError::Parse(
            "SoundCloud short-link redirects are too deep".into(),
        ))
    }

    /// Hydrate playlist entries that the public API returned as ID-only stubs.
    ///
    /// SoundCloud returns full objects for the first few tracks of a large set
    /// and references for the rest. Fetching the remaining tracks in batches
    /// keeps large playlists fast without making one request per track.
    async fn hydrate_playlist(
        &self,
        mut playlist: SoundCloudPlaylist,
        signal: Option<&CancellationToken>,
    ) -> Result<SoundCloudPlaylist> {
        let stubs: Vec<(usize, String)> = playlist
            .tracks
            .iter()
            .enumerate()
            .filter(|(_, track)| track.transcodings.is_empty() && !track.id.is_empty())
            .map(|(index, track)| (index, track.id.clone()))
            .collect();
        if stubs.is_empty() {
            return Ok(playlist);
        }
        let client_id = self.client_id(signal).await?;
        let mut chunks: Vec<Vec<(usize, String)>> = Vec::new();
        for chunk in stubs.chunks(50) {
            chunks.push(chunk.to_vec());
        }
        let mut hydrated: Vec<Option<(usize, SoundCloudTrack)>> = Vec::new();
        let mut calls = stream::iter(chunks.into_iter().map(|chunk| {
            let client = self.clone();
            let client_id = client_id.clone();
            async move {
                let ids = chunk
                    .iter()
                    .map(|(_, id)| id.as_str())
                    .collect::<Vec<_>>()
                    .join(",");
                let endpoint = format!("{API_ORIGIN}/tracks?ids={ids}&client_id={client_id}");
                let raw = client.get_json::<Vec<RawTrack>>(&endpoint, signal).await;
                (chunk, raw)
            }
        }))
        .buffer_unordered(4);
        while let Some((chunk, raw)) = calls.next().await {
            let Ok(raw_tracks) = raw else {
                debug!("SoundCloud playlist hydration chunk failed");
                continue;
            };
            let mut by_id = std::collections::HashMap::new();
            for raw in raw_tracks {
                if let Ok(track) = raw.into_track()
                    && !track.id.is_empty()
                {
                    by_id.insert(track.id.clone(), track);
                }
            }
            for (index, id) in chunk {
                if let Some(track) = by_id.remove(&id) {
                    hydrated.push(Some((index, track)));
                }
            }
        }
        drop(calls);
        for (index, track) in hydrated.into_iter().flatten() {
            if let Some(slot) = playlist.tracks.get_mut(index) {
                *slot = track;
            }
        }

        // If a batch request failed, fall back to individual track lookups for
        // any remaining ID-only stubs. This is slower but keeps large playlists
        // usable through transient batch errors.
        let remaining: Vec<(usize, String)> = playlist
            .tracks
            .iter()
            .enumerate()
            .filter(|(_, track)| track.transcodings.is_empty() && !track.id.is_empty())
            .map(|(index, track)| (index, track.id.clone()))
            .collect();
        if !remaining.is_empty() {
            let mut calls = stream::iter(remaining.into_iter().map(|(index, id)| {
                let client = self.clone();
                let client_id = client_id.clone();
                async move {
                    let endpoint = format!("{API_ORIGIN}/tracks/{id}?client_id={client_id}");
                    let raw = client.get_json::<RawTrack>(&endpoint, signal).await;
                    (index, raw)
                }
            }))
            .buffer_unordered(8);
            while let Some((index, raw)) = calls.next().await {
                if let Ok(track) = raw.and_then(RawTrack::into_track)
                    && let Some(slot) = playlist.tracks.get_mut(index)
                {
                    *slot = track;
                }
            }
        }
        Ok(playlist)
    }

    fn cached_resolved(&self, key: &str) -> Option<SoundCloudEntity> {
        let now = Instant::now();
        let mut entries = self
            .resolved
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match entries.get(key) {
            Some(entry) if now.duration_since(entry.inserted) < Duration::from_secs(120) => {
                Some(entry.entity.clone())
            }
            Some(_) => {
                entries.remove(key);
                None
            }
            None => None,
        }
    }

    fn store_resolved(&self, key: &str, entity: &SoundCloudEntity) {
        let now = Instant::now();
        let mut entries = self
            .resolved
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        entries.retain(|_, entry| now.duration_since(entry.inserted) < Duration::from_secs(120));
        if entries.len() >= 128
            && let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, entry)| entry.inserted)
                .map(|(key, _)| key.clone())
        {
            entries.remove(&oldest);
        }
        entries.insert(
            key.to_owned(),
            CachedEntity {
                entity: entity.clone(),
                inserted: now,
            },
        );
    }

    /// Download one resolved track.
    pub async fn download_track(
        &self,
        track: &SoundCloudTrack,
        quality: QualityPolicy,
        max_bytes: u64,
        max_connections: usize,
        retries: u32,
        signal: Option<&CancellationToken>,
    ) -> Result<SoundCloudAudio> {
        if matches!(track.policy.as_deref(), Some("BLOCK" | "SNIP" | "DENY")) {
            return Err(YsttError::Unavailable(format!(
                "SoundCloud track is not publicly streamable (policy {})",
                track.policy.as_deref().unwrap_or("unknown")
            )));
        }

        let candidates = candidate_transcodings(track, quality);
        if candidates.is_empty() {
            return Err(YsttError::Unavailable(format!(
                "SoundCloud track {} has no supported public stream",
                track.id
            )));
        }
        let mut last_error = None;
        for transcoding in candidates {
            let (mime, ext) = output_format(transcoding);
            match self
                .download_transcoding(
                    track,
                    transcoding,
                    max_bytes,
                    max_connections,
                    retries,
                    signal,
                )
                .await
            {
                Ok((bytes, stream)) => {
                    if bytes.is_empty() {
                        last_error = Some(YsttError::Unavailable(
                            "SoundCloud returned empty audio".into(),
                        ));
                        continue;
                    }
                    if bytes.len() as u64 > max_bytes {
                        last_error = Some(YsttError::FileTooLarge {
                            size: bytes.len() as u64,
                            limit: max_bytes,
                        });
                        continue;
                    }
                    let artwork = match self.download_artwork(track, signal).await {
                        Ok(artwork) => artwork,
                        Err(error) => {
                            debug!(?error, track_id = %track.id, "SoundCloud artwork failed");
                            None
                        }
                    };
                    return Ok(SoundCloudAudio {
                        track: track.clone(),
                        bytes,
                        mime: mime.to_owned(),
                        ext: ext.to_owned(),
                        source_url: stream,
                        preset: transcoding.preset.clone(),
                        artwork,
                    });
                }
                Err(YsttError::Cancelled) => return Err(YsttError::Cancelled),
                Err(error) => {
                    debug!(
                        ?error,
                        track_id = %track.id,
                        preset = transcoding.preset.as_deref().unwrap_or("-"),
                        "SoundCloud stream candidate failed; trying fallback"
                    );
                    last_error = Some(error);
                }
            }
        }
        Err(last_error.unwrap_or_else(|| {
            YsttError::Unavailable(format!(
                "SoundCloud track {} has no working public stream",
                track.id
            ))
        }))
    }

    async fn download_transcoding(
        &self,
        track: &SoundCloudTrack,
        transcoding: &SoundCloudTranscoding,
        max_bytes: u64,
        max_connections: usize,
        retries: u32,
        signal: Option<&CancellationToken>,
    ) -> Result<(Bytes, String)> {
        let stream = self
            .stream_url_with_refresh(track, transcoding, signal)
            .await?;
        let is_hls = transcoding
            .protocol
            .as_deref()
            .is_some_and(|protocol| protocol.eq_ignore_ascii_case("hls"))
            || stream.contains(".m3u8");
        let first = if is_hls {
            self.download_hls(
                &stream,
                max_bytes,
                max_connections.clamp(1, 16),
                retries,
                signal,
            )
            .await
        } else {
            self.download_direct(&stream, max_bytes, retries, signal)
                .await
        };
        match first {
            Ok(bytes) => Ok((bytes, stream)),
            Err(YsttError::Network(NetError::Status {
                status: 401 | 403 | 404,
                ..
            })) => {
                // A signed playback URL may expire between resolve and the
                // first segment. Refresh it once and retry the same audio.
                let refreshed = self
                    .stream_url_with_refresh(track, transcoding, signal)
                    .await?;
                let bytes = if is_hls {
                    self.download_hls(
                        &refreshed,
                        max_bytes,
                        max_connections.clamp(1, 16),
                        retries,
                        signal,
                    )
                    .await?
                } else {
                    self.download_direct(&refreshed, max_bytes, retries, signal)
                        .await?
                };
                Ok((bytes, refreshed))
            }
            Err(error) => Err(error),
        }
    }

    async fn stream_url_with_refresh(
        &self,
        track: &SoundCloudTrack,
        transcoding: &SoundCloudTranscoding,
        signal: Option<&CancellationToken>,
    ) -> Result<String> {
        match self.stream_url(track, transcoding, signal).await {
            Err(YsttError::Network(NetError::Status {
                status: 401 | 403, ..
            })) => {
                self.invalidate_client_id();
                self.stream_url(track, transcoding, signal).await
            }
            Err(YsttError::Network(NetError::Status { status: 404, .. })) => Err(
                YsttError::Unavailable("SoundCloud stream expired or is unavailable".into()),
            ),
            other => other,
        }
    }

    async fn download_artwork(
        &self,
        track: &SoundCloudTrack,
        signal: Option<&CancellationToken>,
    ) -> Result<Option<SoundCloudArtwork>> {
        let Some(url) = track.artwork_url.as_deref() else {
            return Ok(None);
        };
        let mut options = RequestOptions::media().with_retries(1);
        if let Some(signal) = signal {
            options = options.with_signal(signal.clone());
        }
        let response = self.http.get(url, self.media_headers(), options).await?;
        ensure_success(&response, url)?;
        let declared = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("image/jpeg")
            .to_owned();
        let bytes = response
            .bytes(200 * 1024, self.read_timeout, signal)
            .await?;
        let Some(mime) = detect_image_mime(&bytes, &declared) else {
            debug!(track_id = %track.id, %declared, "SoundCloud artwork is not a supported image");
            return Ok(None);
        };
        Ok(Some(SoundCloudArtwork {
            bytes,
            mime: mime.to_owned(),
            url: url.to_owned(),
        }))
    }

    /// Discover and cache a public `client_id`.
    async fn client_id(&self, signal: Option<&CancellationToken>) -> Result<String> {
        if let Some(cached) = self.cached_client_id() {
            return Ok(cached);
        }
        let html = self.get_text(HOME, signal).await?;
        if let Some(id) = capture(&RE_HYDRATED_ID, &html) {
            self.store_client_id(id.clone());
            return Ok(id);
        }
        if let Some(id) = capture(&RE_CLIENT_ID, &html) {
            self.store_client_id(id.clone());
            return Ok(id);
        }

        for script in RE_SCRIPT
            .captures_iter(&html)
            .filter_map(|captures| captures.get(1))
            .map(|value| value.as_str())
            .filter(|url| url.starts_with("https://a-v2.sndcdn.com/"))
            .take(12)
        {
            let body = match self.get_text(script, signal).await {
                Ok(body) => body,
                Err(error) => {
                    debug!(?error, "SoundCloud script fetch failed");
                    continue;
                }
            };
            if let Some(id) = capture(&RE_CLIENT_ID, &body) {
                self.store_client_id(id.clone());
                return Ok(id);
            }
        }

        Err(YsttError::Unavailable(
            "cannot discover a public SoundCloud client_id".into(),
        ))
    }

    fn cached_client_id(&self) -> Option<String> {
        self.client_id
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .map(|cached| cached.id.clone())
    }

    fn store_client_id(&self, id: String) {
        if id.len() < 16
            || id.len() > 128
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        {
            return;
        }
        *self
            .client_id
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(CachedClientId { id });
    }

    fn invalidate_client_id(&self) {
        *self
            .client_id
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }

    async fn stream_url(
        &self,
        track: &SoundCloudTrack,
        transcoding: &SoundCloudTranscoding,
        signal: Option<&CancellationToken>,
    ) -> Result<String> {
        let client_id = self.client_id(signal).await?;
        let mut url = Url::parse(&transcoding.url)
            .map_err(|error| YsttError::Parse(format!("invalid SoundCloud stream URL: {error}")))?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("client_id", &client_id);
            if let Some(authorization) = track.track_authorization.as_deref() {
                query.append_pair("track_authorization", authorization);
            }
        }
        let raw: RawStream = self.get_json(url.as_str(), signal).await?;
        if raw.url.is_empty() {
            return Err(YsttError::Unavailable(
                "SoundCloud stream endpoint returned no URL".into(),
            ));
        }
        Ok(raw.url)
    }

    async fn download_direct(
        &self,
        url: &str,
        max_bytes: u64,
        retries: u32,
        signal: Option<&CancellationToken>,
    ) -> Result<Bytes> {
        let headers = self.media_headers();
        let mut options = RequestOptions::media().with_retries(retries);
        if let Some(signal) = signal {
            options = options.with_signal(signal.clone());
        }
        let response = self.http.get(url, headers, options).await?;
        ensure_success(&response, url)?;
        response
            .bytes(max_bytes, self.read_timeout, signal)
            .await
            .map_err(YsttError::from)
    }

    async fn download_hls(
        &self,
        url: &str,
        max_bytes: u64,
        concurrency: usize,
        retries: u32,
        signal: Option<&CancellationToken>,
    ) -> Result<Bytes> {
        let mut current = url.to_owned();
        for _depth in 0..4 {
            let text = self.get_text(&current, signal).await?;
            let parsed = parse_hls(&text, &current)?;
            if let Some(variant) = parsed.variant {
                current = variant;
                continue;
            }
            if parsed.encrypted {
                return Err(YsttError::Unavailable(
                    "SoundCloud HLS is encrypted; refusing to write broken audio".into(),
                ));
            }
            if parsed.segments.is_empty() {
                return Err(YsttError::Parse("SoundCloud HLS has no segments".into()));
            }

            let init = match parsed.init {
                Some(init) => Some(
                    self.download_segment(&init, max_bytes, retries, signal)
                        .await?,
                ),
                None => None,
            };
            let segments: Vec<String> = parsed.segments;
            let count = segments.len();
            let mut parts: Vec<Option<Bytes>> = vec![None; count];
            let mut downloads =
                stream::iter(segments.into_iter().enumerate().map(|(index, segment)| {
                    let client = self.clone();
                    async move {
                        let result = client
                            .download_segment(&segment, max_bytes, retries, signal)
                            .await;
                        (index, result)
                    }
                }))
                .buffer_unordered(concurrency);
            while let Some((index, result)) = downloads.next().await {
                parts[index] = Some(result?);
            }
            let mut bytes = BytesMut::with_capacity(
                init.as_ref().map_or(0, Bytes::len)
                    + parts.iter().flatten().map(Bytes::len).sum::<usize>(),
            );
            if let Some(init) = init {
                bytes.extend_from_slice(&init);
            }
            for part in parts.into_iter().flatten() {
                bytes.extend_from_slice(&part);
                if bytes.len() as u64 > max_bytes {
                    return Err(YsttError::FileTooLarge {
                        size: bytes.len() as u64,
                        limit: max_bytes,
                    });
                }
            }
            return Ok(bytes.freeze());
        }
        Err(YsttError::Parse(
            "SoundCloud HLS nested playlists are too deep".into(),
        ))
    }

    async fn download_segment(
        &self,
        url: &str,
        max_bytes: u64,
        retries: u32,
        signal: Option<&CancellationToken>,
    ) -> Result<Bytes> {
        let headers = self.media_headers();
        let mut options = RequestOptions::media().with_retries(retries);
        if let Some(signal) = signal {
            options = options.with_signal(signal.clone());
        }
        let response = self.http.get(url, headers, options).await?;
        ensure_success(&response, url)?;
        response
            .bytes(max_bytes, self.read_timeout, signal)
            .await
            .map_err(YsttError::from)
    }

    async fn get_text(&self, url: &str, signal: Option<&CancellationToken>) -> Result<String> {
        let headers = self.json_headers();
        let mut options = RequestOptions::page().with_retries(2);
        if let Some(signal) = signal {
            options = options.with_signal(signal.clone());
        }
        let response = self.http.get(url, headers, options).await?;
        ensure_success(&response, url)?;
        response
            .text(self.page_limit, self.read_timeout, signal)
            .await
            .map_err(YsttError::from)
    }

    async fn get_json<T: DeserializeOwned>(
        &self,
        url: &str,
        signal: Option<&CancellationToken>,
    ) -> Result<T> {
        let headers = self.json_headers();
        let mut options = RequestOptions::page().with_retries(2);
        if let Some(signal) = signal {
            options = options.with_signal(signal.clone());
        }
        let response = self.http.get(url, headers, options).await?;
        ensure_success(&response, url)?;
        let bytes = response
            .bytes(self.page_limit, self.read_timeout, signal)
            .await?;
        serde_json::from_slice(&bytes)
            .map_err(|error| YsttError::Parse(format!("invalid SoundCloud JSON: {error}")))
    }

    fn json_headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        headers.insert(REFERER, HeaderValue::from_static(HOME));
        headers
    }

    fn media_headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("*/*"));
        headers.insert(REFERER, HeaderValue::from_static(HOME));
        headers
    }
}

/// Whether a string is a public SoundCloud track/playlist URL.
pub fn is_soundcloud_url(input: &str) -> bool {
    let Ok(url) = Url::parse(input.trim()) else {
        return false;
    };
    if url.scheme() != "https" {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    matches!(
        host.as_str(),
        "soundcloud.com" | "www.soundcloud.com" | "m.soundcloud.com" | "on.soundcloud.com"
    ) && url.username().is_empty()
        && url.password().is_none()
}

/// Normalize a SoundCloud URL, rejecting non-SoundCloud and non-HTTPS input.
pub fn normalize_url(input: &str) -> Result<String> {
    let input = input.trim();
    if !is_soundcloud_url(input) {
        return Err(YsttError::UnsupportedUrl(format!(
            "not a public SoundCloud URL: {input}"
        )));
    }
    let mut url =
        Url::parse(input).map_err(|error| YsttError::InvalidUrl(format!("{error}: {input}")))?;
    // Share links carry tracking and referral parameters. The canonical
    // permalink is what the public resolve endpoint expects.
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.to_string())
}

fn resolve_endpoint(input: &str, client_id: &str) -> Result<String> {
    let encoded: String = url::form_urlencoded::byte_serialize(input.as_bytes()).collect();
    Ok(format!(
        "{API_ORIGIN}/resolve?url={encoded}&client_id={client_id}"
    ))
}

fn ensure_success(response: &ystt_net::HttpResponse, url: &str) -> Result<()> {
    if response.is_success() {
        return Ok(());
    }
    let status = response.status_u16();
    if matches!(status, 404 | 410) {
        return Err(YsttError::Unavailable(format!(
            "SoundCloud resource unavailable (HTTP {status})"
        )));
    }
    Err(YsttError::Network(NetError::Status {
        status,
        url: url.to_owned(),
    }))
}

fn capture(regex: &Regex, input: &str) -> Option<String> {
    regex
        .captures(input)
        .and_then(|captures| captures.get(1))
        .map(|value| value.as_str().to_owned())
}

static RE_HYDRATED_ID: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#""hydratable"\s*:\s*"apiClient"\s*,\s*"data"\s*:\s*\{\s*"id"\s*:\s*"([^"]+)""#)
        .unwrap()
});
static RE_CLIENT_ID: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
    Regex::new(r#"client_id\s*:\s*"([A-Za-z0-9_-]{16,128})""#).unwrap()
});
static RE_SCRIPT: std::sync::LazyLock<Regex> =
    std::sync::LazyLock::new(|| Regex::new(r#"<script[^>]+src="([^"]+)""#).unwrap());

fn detect_image_mime(bytes: &[u8], declared: &str) -> Option<&'static str> {
    if bytes.len() >= 3 && bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if bytes.len() >= 8 && bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some("image/png");
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    debug!(%declared, bytes = bytes.len(), "unsupported artwork signature");
    None
}

fn output_format(transcoding: &SoundCloudTranscoding) -> (&'static str, &'static str) {
    let preset = transcoding.preset.as_deref().unwrap_or_default();
    let mime = transcoding.mime_type.as_deref().unwrap_or_default();
    if preset.starts_with("aac") || mime.contains("mp4") {
        ("audio/mp4", "m4a")
    } else if preset.starts_with("opus") || mime.contains("ogg") {
        ("audio/ogg", "ogg")
    } else {
        ("audio/mpeg", "mp3")
    }
}

fn candidate_transcodings(
    track: &SoundCloudTrack,
    quality: QualityPolicy,
) -> Vec<&SoundCloudTranscoding> {
    let order: &[&str] = match quality {
        QualityPolicy::Best | QualityPolicy::Balanced | QualityPolicy::Efficient => &[
            "aac_160k", "mp3_1_0", "aac_96k", "mp3_0_1", "opus_1_0", "opus_0_0", "abr_sq",
        ],
        QualityPolicy::Compatible => &[
            "mp3_1_0", "mp3_0_1", "aac_160k", "aac_96k", "opus_1_0", "opus_0_0", "abr_sq",
        ],
        QualityPolicy::Fast => &[
            "aac_96k", "mp3_0_1", "opus_0_0", "aac_160k", "mp3_1_0", "opus_1_0", "abr_sq",
        ],
    };
    let mut candidates = Vec::new();
    for preset in order {
        for transcoding in track.transcodings.iter().filter(|transcoding| {
            transcoding.preset.as_deref() == Some(*preset) && !transcoding.snipped
        }) {
            if !candidates
                .iter()
                .any(|existing: &&SoundCloudTranscoding| existing.url == transcoding.url)
            {
                candidates.push(transcoding);
            }
        }
    }
    for transcoding in track
        .transcodings
        .iter()
        .filter(|transcoding| !transcoding.snipped)
    {
        if !candidates
            .iter()
            .any(|existing: &&SoundCloudTranscoding| existing.url == transcoding.url)
        {
            candidates.push(transcoding);
        }
    }
    candidates
}

#[derive(Default)]
struct ParsedHls {
    init: Option<String>,
    segments: Vec<String>,
    variant: Option<String>,
    encrypted: bool,
}

fn parse_hls(text: &str, base: &str) -> Result<ParsedHls> {
    let base_url = Url::parse(base)
        .map_err(|error| YsttError::Parse(format!("invalid HLS base URL: {error}")))?;
    let mut parsed = ParsedHls::default();
    let lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    let mut pending_variant = false;
    let mut variant_candidates: Vec<(u64, String)> = Vec::new();

    for line in lines {
        if line.starts_with("#EXT-X-KEY") || line.starts_with("#EXT-X-SESSION-KEY") {
            if !line.contains("METHOD=NONE") {
                parsed.encrypted = true;
            }
            continue;
        }
        if line.starts_with("#EXT-X-STREAM-INF") {
            pending_variant = true;
            continue;
        }
        if line.starts_with("#EXT-X-MAP") {
            if let Some(uri) = attribute(line, "URI") {
                parsed.init = Some(resolve_line(&base_url, uri)?);
            }
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        if pending_variant {
            pending_variant = false;
            let bandwidth = text
                .lines()
                .find(|candidate| candidate.contains("BANDWIDTH="))
                .and_then(|candidate| attribute(candidate, "BANDWIDTH"))
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(0);
            variant_candidates.push((bandwidth, resolve_line(&base_url, line)?));
            continue;
        }
        parsed.segments.push(resolve_line(&base_url, line)?);
    }

    if !variant_candidates.is_empty() {
        // Pick the highest variant; SoundCloud generally exposes a single
        // audio rendition per media endpoint, but this keeps master playlists
        // deterministic and high quality.
        variant_candidates.sort_by_key(|(bandwidth, _)| *bandwidth);
        parsed.variant = variant_candidates.pop().map(|(_, url)| url);
        parsed.segments.clear();
    }
    Ok(parsed)
}

fn attribute<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let prefix = format!("{name}=");
    let start = line.find(&prefix)? + prefix.len();
    let rest = &line[start..];
    if let Some(stripped) = rest.strip_prefix('"') {
        stripped.split('"').next()
    } else {
        rest.split(',').next().map(str::trim)
    }
}

fn resolve_line(base: &Url, line: &str) -> Result<String> {
    base.join(line)
        .map(|url| url.to_string())
        .map_err(|error| YsttError::Parse(format!("invalid HLS URI: {error}")))
}

#[derive(Debug, Deserialize)]
struct RawStream {
    url: String,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum IdValue {
    String(String),
    Number(u64),
}

impl IdValue {
    fn into_string(self) -> String {
        match self {
            Self::String(value) => value,
            Self::Number(value) => value.to_string(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawEntity {
    kind: Option<String>,
    id: Option<IdValue>,
    title: Option<String>,
    duration: Option<u64>,
    policy: Option<String>,
    track_authorization: Option<String>,
    artwork_url: Option<String>,
    permalink_url: Option<String>,
    description: Option<String>,
    genre: Option<String>,
    user: Option<RawUser>,
    media: Option<RawMedia>,
    tracks: Option<Vec<Option<RawTrack>>>,
}

impl RawEntity {
    fn into_entity(self) -> Result<SoundCloudEntity> {
        match self.kind.as_deref() {
            Some("playlist") => {
                let tracks: Vec<SoundCloudTrack> = self
                    .tracks
                    .unwrap_or_default()
                    .into_iter()
                    .flatten()
                    .filter_map(|track| track.into_track().ok())
                    .collect();
                if tracks.is_empty() {
                    return Err(YsttError::Unavailable(
                        "SoundCloud playlist contains no public tracks".into(),
                    ));
                }
                let artwork_url = self
                    .artwork_url
                    .map(|url| normalize_artwork(&url))
                    .or_else(|| tracks.first().and_then(|track| track.artwork_url.clone()));
                Ok(SoundCloudEntity::Playlist(SoundCloudPlaylist {
                    id: self.id.map(IdValue::into_string).unwrap_or_default(),
                    title: self.title.unwrap_or_else(|| "SoundCloud playlist".into()),
                    artist: self
                        .user
                        .as_ref()
                        .and_then(|user| user.username.clone())
                        .unwrap_or_else(|| "SoundCloud".into()),
                    permalink_url: self.permalink_url.unwrap_or_default(),
                    description: self.description,
                    duration_ms: self.duration,
                    artwork_url,
                    tracks,
                }))
            }
            _ => {
                let raw = RawTrack {
                    id: self.id,
                    title: self.title,
                    duration: self.duration,
                    policy: self.policy,
                    track_authorization: self.track_authorization,
                    artwork_url: self.artwork_url,
                    permalink_url: self.permalink_url,
                    description: self.description,
                    genre: self.genre,
                    user: self.user,
                    media: self.media,
                };
                Ok(SoundCloudEntity::Track(raw.into_track()?))
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawTrack {
    id: Option<IdValue>,
    title: Option<String>,
    duration: Option<u64>,
    policy: Option<String>,
    track_authorization: Option<String>,
    artwork_url: Option<String>,
    permalink_url: Option<String>,
    description: Option<String>,
    genre: Option<String>,
    user: Option<RawUser>,
    media: Option<RawMedia>,
}

impl RawTrack {
    fn into_track(self) -> Result<SoundCloudTrack> {
        let policy = self.policy;
        if matches!(policy.as_deref(), Some("BLOCK" | "SNIP" | "DENY")) {
            return Err(YsttError::Unavailable(format!(
                "SoundCloud track is not publicly streamable (policy {})",
                policy.as_deref().unwrap_or("unknown")
            )));
        }
        let user = self.user;
        let avatar = user.as_ref().and_then(|user| user.avatar_url.as_deref());
        let artwork_url = self
            .artwork_url
            .as_deref()
            .or(avatar)
            .map(normalize_artwork);
        let transcodings = self
            .media
            .map(|media| media.transcodings)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|transcoding| {
                let url = transcoding.url?;
                let format = transcoding.format.unwrap_or_default();
                Some(SoundCloudTranscoding {
                    preset: transcoding.preset,
                    url,
                    protocol: format.protocol,
                    mime_type: format.mime_type,
                    snipped: transcoding.snipped.unwrap_or(false),
                })
            })
            .collect::<Vec<_>>();
        Ok(SoundCloudTrack {
            id: self.id.map(IdValue::into_string).unwrap_or_default(),
            title: self.title.unwrap_or_else(|| "SoundCloud track".into()),
            artist: user
                .as_ref()
                .and_then(|user| user.username.clone())
                .unwrap_or_else(|| "SoundCloud".into()),
            artist_id: user.and_then(|user| user.id).map(IdValue::into_string),
            duration_ms: self.duration,
            permalink_url: self.permalink_url.unwrap_or_default(),
            artwork_url,
            policy,
            description: self.description,
            genre: self.genre,
            transcodings,
            track_authorization: self.track_authorization,
        })
    }
}

#[derive(Debug, Deserialize)]
struct RawUser {
    id: Option<IdValue>,
    username: Option<String>,
    avatar_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawMedia {
    #[serde(default)]
    transcodings: Vec<RawTranscoding>,
}

#[derive(Debug, Deserialize)]
struct RawTranscoding {
    preset: Option<String>,
    url: Option<String>,
    format: Option<RawFormat>,
    snipped: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct RawFormat {
    protocol: Option<String>,
    mime_type: Option<String>,
}

fn normalize_artwork(url: &str) -> String {
    let url = url.replace("-large.", "-t300x300.");
    let url = url.replace("-t500x500.", "-t300x300.");
    url.replace("-crop.", "-t300x300.")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(preset: &str, protocol: &str, mime: &str) -> SoundCloudTranscoding {
        SoundCloudTranscoding {
            preset: Some(preset.into()),
            url: format!("https://api.test/{preset}"),
            protocol: Some(protocol.into()),
            mime_type: Some(mime.into()),
            snipped: false,
        }
    }

    #[test]
    fn detects_only_public_soundcloud_hosts() {
        assert!(is_soundcloud_url("https://soundcloud.com/user/track?si=x"));
        assert!(is_soundcloud_url("https://on.soundcloud.com/abc"));
        assert!(!is_soundcloud_url("http://soundcloud.com/user/track"));
        assert!(!is_soundcloud_url("https://soundcloud.com.evil.test/x"));
        assert!(!is_soundcloud_url("https://example.com/x"));
    }

    #[test]
    fn normalize_strips_share_query() {
        assert_eq!(
            normalize_url("https://soundcloud.com/u/t?si=1&utm_source=x").unwrap(),
            "https://soundcloud.com/u/t"
        );
    }

    #[test]
    fn artwork_is_square_small() {
        assert_eq!(
            normalize_artwork("https://i1.sndcdn.com/artworks-x-large.jpg"),
            "https://i1.sndcdn.com/artworks-x-t300x300.jpg"
        );
        assert_eq!(
            normalize_artwork("https://i1.sndcdn.com/artworks-x-t500x500.jpg"),
            "https://i1.sndcdn.com/artworks-x-t300x300.jpg"
        );
    }

    #[test]
    fn transcoding_quality_order_matches_policy() {
        let track = SoundCloudTrack {
            id: "1".into(),
            title: "t".into(),
            artist: "a".into(),
            artist_id: None,
            duration_ms: None,
            permalink_url: "https://soundcloud.com/u/t".into(),
            artwork_url: None,
            policy: Some("MONETIZE".into()),
            description: None,
            genre: None,
            transcodings: vec![
                track("aac_96k", "hls", "audio/mp4"),
                track("aac_160k", "hls", "audio/mp4"),
                track("mp3_1_0", "progressive", "audio/mpeg"),
            ],
            track_authorization: None,
        };
        assert_eq!(
            candidate_transcodings(&track, QualityPolicy::Best)[0]
                .preset
                .as_deref(),
            Some("aac_160k")
        );
        assert_eq!(
            candidate_transcodings(&track, QualityPolicy::Fast)[0]
                .preset
                .as_deref(),
            Some("aac_96k")
        );
        assert_eq!(
            candidate_transcodings(&track, QualityPolicy::Compatible)[0]
                .preset
                .as_deref(),
            Some("mp3_1_0")
        );
    }

    #[test]
    fn hls_parser_reads_map_and_segments() {
        let text =
            "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:5,\nseg0.m4s\n#EXTINF:5,\nseg1.m4s\n";
        let parsed = parse_hls(text, "https://cdn.test/path/playlist.m3u8").unwrap();
        assert_eq!(
            parsed.init.as_deref(),
            Some("https://cdn.test/path/init.mp4")
        );
        assert_eq!(
            parsed.segments,
            vec![
                "https://cdn.test/path/seg0.m4s",
                "https://cdn.test/path/seg1.m4s"
            ]
        );
        assert!(!parsed.encrypted);
    }

    #[test]
    fn hls_parser_rejects_encryption() {
        let text = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"key\"\n#EXTINF:5,\nseg0.m4s\n";
        let parsed = parse_hls(text, "https://cdn.test/p.m3u8").unwrap();
        assert!(parsed.encrypted);
    }

    #[test]
    fn hls_master_selects_highest_variant() {
        let text = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=128000,CODECS=\"mp4a.40.2\"\nlow.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=256000,CODECS=\"mp4a.40.2\"\nhigh.m3u8\n";
        let parsed = parse_hls(text, "https://cdn.test/master.m3u8").unwrap();
        assert_eq!(
            parsed.variant.as_deref(),
            Some("https://cdn.test/high.m3u8")
        );
        assert!(parsed.segments.is_empty());
    }

    #[test]
    fn output_formats_cover_aac_mp3_and_opus() {
        assert_eq!(
            output_format(&track("aac_160k", "hls", "audio/mp4")),
            ("audio/mp4", "m4a")
        );
        assert_eq!(
            output_format(&track("mp3_1_0", "progressive", "audio/mpeg")),
            ("audio/mpeg", "mp3")
        );
        assert_eq!(
            output_format(&track("opus_1_0", "hls", "audio/ogg")),
            ("audio/ogg", "ogg")
        );
    }

    #[test]
    fn raw_track_converts_and_rejects_blocked_policy() {
        let raw: RawTrack = serde_json::from_str(
            r##"{"id":1,"title":"T","duration":1234,"policy":"BLOCK","user":{"username":"U"},"media":{"transcodings":[]}}"##,
        )
        .unwrap();
        assert!(matches!(raw.into_track(), Err(YsttError::Unavailable(_))));
    }
}
