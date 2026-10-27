//! Request and response types shared by handlers and OpenAPI.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use ystt::{Author, DownloadOptions, MediaInfo, MediaKind, PartialFailure, QualityPolicy, Timings};

use crate::config::ApiConfig;
use crate::error::{ApiError, ErrorBody};
use crate::store::MediaStats;

/// Per-request download policy.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
pub struct DownloadOptionsInput {
    /// Video stream policy. Defaults to the server policy.
    pub quality: Option<QualityPolicy>,
    /// Include the post's music track.
    pub include_audio: Option<bool>,
    /// Allow individual gallery/audio failures.
    pub allow_partial: Option<bool>,
    /// Maximum bytes for any one media item.
    pub max_bytes: Option<u64>,
    /// Use parallel CDN ranges for large videos.
    pub parallel_ranges: Option<bool>,
    /// Maximum range connections.
    pub max_connections: Option<usize>,
    /// Maximum gallery concurrency.
    pub photo_concurrency: Option<usize>,
    /// Adapt range concurrency while downloading.
    pub adaptive_ranges: Option<bool>,
    /// Per-media retry budget.
    pub media_retries: Option<u32>,
}

impl DownloadOptionsInput {
    /// Merge request overrides with server limits.
    pub fn to_options(&self, config: &ApiConfig) -> Result<DownloadOptions, ApiError> {
        let max_bytes = self.max_bytes.unwrap_or(config.max_bytes);
        if max_bytes > config.max_bytes {
            return Err(ApiError::new(
                axum::http::StatusCode::PAYLOAD_TOO_LARGE,
                "file_too_large",
                format!(
                    "max_bytes {max_bytes} exceeds server limit {}",
                    config.max_bytes
                ),
            ));
        }
        let mut options = DownloadOptions::new().max_bytes(max_bytes);
        if let Some(value) = self.quality {
            options = options.quality(value);
        }
        if let Some(value) = self.include_audio {
            options = options.include_audio(value);
        }
        if let Some(value) = self.allow_partial {
            options = options.allow_partial(value);
        }
        if let Some(value) = self.parallel_ranges {
            options = options.parallel_ranges(value);
        }
        if let Some(value) = self.max_connections {
            options = options.max_connections(value);
        }
        if let Some(value) = self.photo_concurrency {
            options = options.photo_concurrency(value);
        }
        if let Some(value) = self.adaptive_ranges {
            options = options.adaptive_ranges(value);
        }
        if let Some(value) = self.media_retries {
            options = options.media_retries(value);
        }
        Ok(options)
    }
}

/// Resolve a TikTok URL without downloading media.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ResolveRequest {
    /// TikTok post, short-link or share URL.
    pub url: String,
    /// Resolver quality policy.
    #[serde(flatten)]
    pub options: DownloadOptionsInput,
}

/// Download one post.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct DownloadRequest {
    /// TikTok post, short-link or share URL.
    pub url: String,
    /// Per-request download policy.
    #[serde(flatten)]
    pub options: DownloadOptionsInput,
}

/// Download several posts with bounded concurrency.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct BatchRequest {
    /// URLs in input order.
    pub urls: Vec<String>,
    /// Per-item concurrency (server-capped).
    pub concurrency: Option<usize>,
    /// Shared per-request download policy.
    #[serde(flatten)]
    pub options: DownloadOptionsInput,
}

/// One media object in the response. Fetch bytes from `url` with the same Bearer key.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct MediaDescriptor {
    /// Short-lived opaque media token.
    pub token: String,
    /// Authenticated media URL.
    pub url: String,
    /// Media kind.
    pub kind: MediaKind,
    /// MIME type.
    pub mime: String,
    /// Extension without a dot.
    pub ext: String,
    /// Size in bytes.
    pub size: u64,
    /// SHA-256 of the payload.
    pub sha256: String,
    /// Original CDN URL.
    pub source_url: String,
    /// Width for video/photo media.
    pub width: Option<u32>,
    /// Height for video/photo media.
    pub height: Option<u32>,
    /// Duration for video media.
    pub duration_sec: Option<u32>,
}

/// Full result of an in-memory download.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DownloadResponse {
    /// Post ID.
    pub post_id: String,
    /// Original input URL.
    pub source_url: String,
    /// Canonical TikTok page URL.
    pub page_url: String,
    /// Author.
    pub author: Author,
    /// Caption.
    pub description: Option<String>,
    /// Cover image URL.
    pub cover_url: Option<String>,
    /// Video when present.
    pub video: Option<MediaDescriptor>,
    /// Gallery photos in order.
    pub photos: Vec<MediaDescriptor>,
    /// Music when requested.
    pub audio: Option<MediaDescriptor>,
    /// Partial failures.
    pub failures: Vec<PartialFailure>,
    /// Stage timings.
    pub timings: Timings,
    /// Always false for the in-memory API.
    pub cache_hit: bool,
}

/// One batch item.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BatchItem {
    /// Zero-based input index.
    pub index: usize,
    /// Input URL.
    pub url: String,
    /// Success result.
    pub result: Option<DownloadResponse>,
    /// Failure details.
    pub error: Option<ErrorBody>,
}

/// Batch response in input order.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BatchResponse {
    /// Ordered items.
    pub items: Vec<BatchItem>,
    /// Successful item count.
    pub successes: usize,
    /// Failed item count.
    pub failures: usize,
    /// End-to-end milliseconds.
    pub total_ms: u64,
}

/// Resolve response.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ResolveResponse {
    /// Resolved media metadata.
    #[serde(flatten)]
    pub info: MediaInfo,
}

/// Liveness/readiness response.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct HealthResponse {
    /// Service state.
    pub status: String,
    /// Crate version.
    pub version: String,
}

/// Runtime cache/queue statistics.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct StatsResponse {
    /// Bounded media store.
    pub media: MediaStats,
    /// Maximum concurrent downloads.
    pub workers: usize,
    /// Maximum batch concurrency.
    pub batch_concurrency: usize,
    /// Configured media TTL in seconds.
    pub media_ttl_secs: u64,
}
