//! Builder for configuring a downloader.

use std::path::PathBuf;
use std::time::Duration;

use crate::config::{QualityPolicy, YsttConfig};
use crate::error::Result;

use super::Ystt;

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
        self.config.output.dir = path.into();
        self
    }

    /// Default quality policy.
    pub fn quality(mut self, quality: QualityPolicy) -> Self {
        self.config.output.quality = quality;
        self
    }

    /// Default output filename template.
    pub fn filename_template(mut self, template: impl Into<String>) -> Self {
        self.config.output.filename_template = Some(template.into());
        self
    }

    /// Download music tracks.
    ///
    /// Audio uses the same pipeline and ALPN Auto transport as video; H1-only
    /// music hosts fall back to HTTP/1.1 automatically.
    pub fn include_audio(mut self, include: bool) -> Self {
        self.config.output.include_audio = include;
        self
    }

    /// Enable range-first parallel downloads.
    pub fn parallel_ranges(mut self, enabled: bool) -> Self {
        self.config.media.parallel_ranges = enabled;
        self
    }

    /// Maximum range connections.
    pub fn max_connections(mut self, connections: usize) -> Self {
        self.config.media.max_connections = connections.max(1);
        self
    }

    /// Maximum photo concurrency.
    pub fn photo_concurrency(mut self, concurrency: usize) -> Self {
        self.config.media.photo_concurrency = concurrency.max(1);
        self
    }

    /// Maximum media size.
    pub fn max_bytes(mut self, max: u64) -> Self {
        self.config.media.max_bytes = max;
        self
    }

    /// Maximum page/JSON size.
    pub fn max_page_bytes(mut self, max: u64) -> Self {
        self.config.network.max_page_bytes = max;
        self
    }

    /// Browser fingerprint profile.
    pub fn browser(mut self, browser: ystt_net::BrowserProfile) -> Self {
        self.config.network.http.browser = browser;
        self
    }

    /// HTTP transport policy (`Auto`, `Http2Only`, `Http1Only`).
    pub fn transport(mut self, transport: ystt_net::TransportPolicy) -> Self {
        self.config.network.http.transport = transport;
        self
    }

    /// Optional HTTP/2 tuning that preserves TLS/JA3/JA4 and H2 settings.
    pub fn http2_tuning(mut self, tuning: ystt_net::Http2Tuning) -> Self {
        self.config.network.http.http2 = tuning;
        self
    }

    /// Maximum idle connections kept per host.
    pub fn pool_max_idle_per_host(mut self, count: usize) -> Self {
        self.config.network.http.pool_max_idle_per_host = count;
        self
    }

    /// Total request timeout.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.config.network.http.timeout = timeout;
        self
    }

    /// Connection timeout.
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.config.network.http.connect_timeout = timeout;
        self
    }

    /// Body idle timeout.
    pub fn read_timeout(mut self, timeout: Duration) -> Self {
        self.config.network.http.read_timeout = timeout;
        self
    }

    /// HTTP retry count.
    pub fn retries(mut self, retries: u32) -> Self {
        self.config.network.http.retries = retries;
        self
    }

    /// Page concurrency per host.
    pub fn per_host_concurrency(mut self, concurrency: usize) -> Self {
        self.config.network.http.per_host_concurrency = concurrency.max(1);
        self
    }

    /// Minimum spacing between page requests.
    pub fn min_request_interval(mut self, interval: Duration) -> Self {
        self.config.network.http.min_request_interval = interval;
        self
    }

    /// Override the emulated user-agent.
    pub fn user_agent(mut self, user_agent: impl Into<String>) -> Self {
        self.config.network.http.user_agent = Some(user_agent.into());
        self
    }

    /// Disable environment/system proxies.
    pub fn no_proxy(mut self, disabled: bool) -> Self {
        self.config.network.http.no_proxy = disabled;
        self
    }

    /// Enable or disable the lightweight embed resolver.
    pub fn use_embed(mut self, enabled: bool) -> Self {
        self.config.resolver.use_embed = enabled;
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
        self.config.cache.url_ttl = ttl;
        self
    }

    /// Reuse existing output files.
    pub fn reuse_existing(mut self, reuse: bool) -> Self {
        self.config.output.reuse_existing = reuse;
        self
    }

    /// Enable or disable resumable video downloads.
    pub fn resume(mut self, enabled: bool) -> Self {
        self.config.media.resume = enabled;
        self
    }

    /// Enable or disable the content-addressed file cache.
    pub fn file_cache(mut self, enabled: bool) -> Self {
        self.config.cache.file = enabled;
        self
    }

    /// Override the file-cache root.
    pub fn file_cache_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.config.cache.file_dir = Some(path.into());
        self
    }

    /// Shared cache root for file and metadata caches across output directories.
    pub fn cache_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.config.cache.dir = Some(path.into());
        self
    }

    /// File-cache TTL.
    pub fn file_cache_ttl(mut self, ttl: Duration) -> Self {
        self.config.cache.file_ttl = ttl;
        self
    }

    /// Enable or disable the persistent metadata cache.
    pub fn metadata_cache(mut self, enabled: bool) -> Self {
        self.config.cache.metadata = enabled;
        self
    }

    /// Override the metadata-cache root.
    pub fn metadata_cache_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.config.cache.metadata_dir = Some(path.into());
        self
    }

    /// Metadata-cache TTL ceiling.
    pub fn metadata_cache_ttl(mut self, ttl: Duration) -> Self {
        self.config.cache.metadata_ttl = ttl;
        self
    }

    /// Advanced: override the page origin (local integration tests/mirrors).
    pub fn page_origin(mut self, origin: impl Into<String>) -> Self {
        self.config.resolver.page_origin = Some(origin.into().trim_end_matches('/').to_owned());
        self
    }

    /// Retry budget for each media item.
    pub fn media_retries(mut self, retries: u32) -> Self {
        self.config.media.retries = retries;
        self
    }

    /// Allow partial gallery results.
    pub fn allow_partial(mut self, allow: bool) -> Self {
        self.config.output.allow_partial = allow;
        self
    }

    /// Enable adaptive range concurrency.
    pub fn adaptive_ranges(mut self, enabled: bool) -> Self {
        self.config.media.adaptive_ranges = enabled;
        self
    }

    /// Initial range connections when adaptive mode is enabled.
    pub fn range_start_connections(mut self, connections: usize) -> Self {
        self.config.media.range_start_connections = connections.max(1);
        self
    }

    /// First range size for videos.
    pub fn range_first_chunk(mut self, bytes: u64) -> Self {
        self.config.media.range_first_chunk = bytes.max(1);
        self
    }

    /// Minimum follow-up range size.
    pub fn range_min_chunk(mut self, bytes: u64) -> Self {
        self.config.media.range_min_chunk = bytes.max(1);
        self
    }

    /// Files at or below this size use one normal GET instead of ranges.
    pub fn range_threshold(mut self, bytes: u64) -> Self {
        self.config.media.range_threshold = bytes;
        self
    }

    /// Build the downloader.
    pub fn build(self) -> Result<Ystt> {
        Ystt::new(self.config)
    }
}
