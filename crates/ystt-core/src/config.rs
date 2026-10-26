//! High-level configuration and per-call download options.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use ystt_net::HttpClientConfig;
use ystt_waf::WafSolverConfig;

/// How to choose a video stream when a post offers several bitrates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum QualityPolicy {
    /// Highest bitrate. Best visual quality, largest file.
    Best,
    /// Prefer H.264/AVC for maximum player compatibility, then highest bitrate.
    Compatible,
    /// H.264 within 1080p/1920px, highest bitrate; good first-frame latency and quality.
    #[default]
    Balanced,
    /// Lowest usable bitrate. Smallest file and fastest time to finish.
    Fast,
}

impl QualityPolicy {
    /// Stable name for cache keys and JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Best => "best",
            Self::Compatible => "compatible",
            Self::Balanced => "balanced",
            Self::Fast => "fast",
        }
    }
}

/// Downloader-wide configuration.
#[derive(Debug, Clone)]
pub struct YsttConfig {
    /// Default output directory.
    pub output_dir: PathBuf,
    /// Default quality policy.
    pub quality: QualityPolicy,
    /// Download the post's music track in addition to video/photos.
    ///
    /// Music is fetched through the normal ALPN Auto transport; H1-only audio
    /// hosts such as TikTok's `v45` work through the HTTP/1.1 fallback.
    pub include_audio: bool,
    /// Use the range-first parallel downloader for videos.
    pub parallel_ranges: bool,
    /// Maximum simultaneous range connections for one video.
    pub max_connections: usize,
    /// Maximum simultaneous photo downloads.
    pub photo_concurrency: usize,
    /// Maximum size of one media file.
    pub max_bytes: u64,
    /// Maximum size of an HTML page or JSON payload.
    pub max_page_bytes: u64,
    /// HTTP client configuration.
    pub http: HttpClientConfig,
    /// WAF solver configuration.
    pub waf: WafSolverConfig,
    /// Default signed-URL cache TTL.
    pub cache_ttl: Duration,
    /// Reuse an existing output file without a network request.
    pub reuse_existing: bool,
    /// Enable the content-addressed file cache.
    pub file_cache: bool,
    /// Optional file-cache root. `None` means `<output_dir>/.ystt-cache`.
    pub file_cache_dir: Option<PathBuf>,
    /// File-cache TTL.
    pub file_cache_ttl: Duration,
    /// Try the lightweight embed resolver before the full page.
    pub use_embed: bool,
    /// Advanced: override the page origin (for local integration tests or mirrors).
    pub page_origin: Option<String>,
    /// Retry budget for each media item.
    pub media_retries: u32,
    /// Return successful gallery photos and list failures instead of failing all.
    pub allow_partial: bool,
    /// Adapt range concurrency while downloading.
    pub adaptive_ranges: bool,
    /// Initial connections for adaptive range downloads.
    pub range_start_connections: usize,
    /// First range size for the range-first downloader.
    pub range_first_chunk: u64,
    /// Minimum size of a follow-up range.
    pub range_min_chunk: u64,
    /// Files at or below this size use one normal GET instead of ranges.
    pub range_threshold: u64,
}

impl Default for YsttConfig {
    fn default() -> Self {
        Self {
            output_dir: PathBuf::from("downloads"),
            quality: QualityPolicy::default(),
            include_audio: false,
            parallel_ranges: true,
            max_connections: 6,
            photo_concurrency: 12,
            max_bytes: 512 * 1024 * 1024,
            max_page_bytes: 4 * 1024 * 1024,
            http: HttpClientConfig::default(),
            waf: WafSolverConfig::default(),
            cache_ttl: Duration::from_secs(10 * 60),
            reuse_existing: false,
            file_cache: true,
            file_cache_dir: None,
            file_cache_ttl: Duration::from_secs(24 * 60 * 60),
            page_origin: None,
            use_embed: false,
            media_retries: 2,
            allow_partial: true,
            adaptive_ranges: true,
            range_start_connections: 2,
            range_first_chunk: 8 * 1024 * 1024,
            range_min_chunk: 2 * 1024 * 1024,
            range_threshold: 64 * 1024 * 1024,
        }
    }
}

/// Per-call overrides. `None` means "use [`YsttConfig`]".
#[derive(Clone, Default)]
pub struct DownloadOptions {
    /// Output directory override.
    pub output_dir: Option<PathBuf>,
    /// Quality policy override.
    pub quality: Option<QualityPolicy>,
    /// Include the music track.
    pub include_audio: Option<bool>,
    /// Enable or disable range-first downloads.
    pub parallel_ranges: Option<bool>,
    /// Maximum range connections.
    pub max_connections: Option<usize>,
    /// Maximum photo concurrency.
    pub photo_concurrency: Option<usize>,
    /// Maximum media size.
    pub max_bytes: Option<u64>,
    /// Reuse an existing file.
    pub reuse_existing: Option<bool>,
    /// Enable or disable the file cache.
    pub file_cache: Option<bool>,
    /// Retry budget per media item.
    pub media_retries: Option<u32>,
    /// Allow partial gallery results.
    pub allow_partial: Option<bool>,
    /// Adapt range concurrency while downloading.
    pub adaptive_ranges: Option<bool>,
    /// Progress reporter.
    pub progress: Option<crate::progress::ProgressReporter>,
    /// Cancellation token.
    pub signal: Option<tokio_util::sync::CancellationToken>,
}

impl std::fmt::Debug for DownloadOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DownloadOptions")
            .field("output_dir", &self.output_dir)
            .field("quality", &self.quality)
            .field("include_audio", &self.include_audio)
            .field("parallel_ranges", &self.parallel_ranges)
            .field("max_connections", &self.max_connections)
            .field("photo_concurrency", &self.photo_concurrency)
            .field("max_bytes", &self.max_bytes)
            .field("reuse_existing", &self.reuse_existing)
            .field("file_cache", &self.file_cache)
            .field("media_retries", &self.media_retries)
            .field("allow_partial", &self.allow_partial)
            .field("adaptive_ranges", &self.adaptive_ranges)
            .field("progress", &self.progress.is_some())
            .field("signal", &self.signal.is_some())
            .finish()
    }
}

impl DownloadOptions {
    /// Create an empty override set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the output directory.
    pub fn output_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.output_dir = Some(path.into());
        self
    }

    /// Set the quality policy.
    pub fn quality(mut self, quality: QualityPolicy) -> Self {
        self.quality = Some(quality);
        self
    }

    /// Include or exclude the music track.
    pub fn include_audio(mut self, include: bool) -> Self {
        self.include_audio = Some(include);
        self
    }

    /// Enable or disable range-first downloads.
    pub fn parallel_ranges(mut self, enabled: bool) -> Self {
        self.parallel_ranges = Some(enabled);
        self
    }

    /// Set the maximum number of range connections.
    pub fn max_connections(mut self, connections: usize) -> Self {
        self.max_connections = Some(connections);
        self
    }

    /// Set the photo concurrency.
    pub fn photo_concurrency(mut self, concurrency: usize) -> Self {
        self.photo_concurrency = Some(concurrency);
        self
    }

    /// Set the maximum media size.
    pub fn max_bytes(mut self, max: u64) -> Self {
        self.max_bytes = Some(max);
        self
    }

    /// Reuse an existing file.
    pub fn reuse_existing(mut self, reuse: bool) -> Self {
        self.reuse_existing = Some(reuse);
        self
    }

    /// Enable or disable the file cache.
    pub fn file_cache(mut self, enabled: bool) -> Self {
        self.file_cache = Some(enabled);
        self
    }

    /// Set the per-media retry budget.
    pub fn media_retries(mut self, retries: u32) -> Self {
        self.media_retries = Some(retries);
        self
    }

    /// Allow partial gallery results.
    pub fn allow_partial(mut self, allow: bool) -> Self {
        self.allow_partial = Some(allow);
        self
    }

    /// Enable or disable adaptive range concurrency.
    pub fn adaptive_ranges(mut self, enabled: bool) -> Self {
        self.adaptive_ranges = Some(enabled);
        self
    }

    /// Attach a progress reporter.
    pub fn progress(mut self, reporter: crate::progress::ProgressReporter) -> Self {
        self.progress = Some(reporter);
        self
    }

    /// Attach a cancellation token.
    pub fn signal(mut self, signal: tokio_util::sync::CancellationToken) -> Self {
        self.signal = Some(signal);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let config = YsttConfig::default();
        assert_eq!(config.output_dir, PathBuf::from("downloads"));
        assert_eq!(config.quality, QualityPolicy::Balanced);
        assert!(config.parallel_ranges);
        assert_eq!(config.max_connections, 6);
        assert_eq!(config.range_threshold, 64 * 1024 * 1024);
    }

    #[test]
    fn option_builder_is_typed() {
        let options = DownloadOptions::new()
            .output_dir("out")
            .quality(QualityPolicy::Fast)
            .max_connections(3)
            .include_audio(true);
        assert_eq!(options.output_dir, Some(PathBuf::from("out")));
        assert_eq!(options.quality, Some(QualityPolicy::Fast));
        assert_eq!(options.max_connections, Some(3));
        assert_eq!(options.include_audio, Some(true));
    }
}
