//! Public result and batch types for the download pipeline.

use std::path::PathBuf;

use bytes::Bytes;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::config::{DownloadOptions, QualityPolicy};
use crate::error::{ErrorInfo, PartialFailure};
use crate::media::DownloadedFile;
use crate::progress::ProgressReporter;

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

/// One video held fully in memory.
#[derive(Debug, Clone)]
pub struct InMemoryVideo {
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
    /// Video width, when known.
    pub width: Option<u32>,
    /// Video height, when known.
    pub height: Option<u32>,
    /// Duration in seconds, when known.
    pub duration_sec: Option<u32>,
    /// Cover image URL, when known.
    pub cover_url: Option<String>,
    /// MIME type of the payload.
    pub mime: String,
    /// Extension without a dot.
    pub ext: String,
    /// Source CDN URL.
    pub media_url: String,
    /// Complete video payload.
    pub bytes: Bytes,
    /// Stage timings.
    pub timings: Timings,
}

/// Options for one batch download job.
#[derive(Clone)]
pub struct BatchOptions {
    /// Maximum number of URLs downloaded concurrently (capped at 64).
    pub concurrency: usize,
    /// Per-item download overrides shared by every URL.
    pub download: DownloadOptions,
    /// Progress reporter for batch and item events.
    pub progress: Option<ProgressReporter>,
    /// Cancellation token shared by every item.
    pub signal: Option<CancellationToken>,
    /// Per-item cancellation tokens, keyed by input index.
    pub item_signals: std::collections::HashMap<usize, CancellationToken>,
}

impl std::fmt::Debug for BatchOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BatchOptions")
            .field("concurrency", &self.concurrency)
            .field("download", &self.download)
            .field("progress", &self.progress.is_some())
            .field("signal", &self.signal.is_some())
            .field("item_signals", &self.item_signals.len())
            .finish()
    }
}

impl Default for BatchOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl BatchOptions {
    /// Create options with a conservative default concurrency of four.
    pub fn new() -> Self {
        Self {
            concurrency: 4,
            download: DownloadOptions::new(),
            progress: None,
            signal: None,
            item_signals: std::collections::HashMap::new(),
        }
    }

    /// Set the maximum number of concurrent downloads.
    pub fn concurrency(mut self, concurrency: usize) -> Self {
        self.concurrency = concurrency.max(1);
        self
    }

    /// Set the output directory.
    pub fn output_dir(mut self, output_dir: impl Into<PathBuf>) -> Self {
        self.download.output_dir = Some(output_dir.into());
        self
    }

    /// Set the quality policy.
    pub fn quality(mut self, quality: QualityPolicy) -> Self {
        self.download.quality = Some(quality);
        self
    }

    /// Set the output filename template.
    pub fn filename_template(mut self, template: impl Into<String>) -> Self {
        self.download.filename_template = Some(template.into());
        self
    }

    /// Include or exclude music.
    pub fn include_audio(mut self, include: bool) -> Self {
        self.download.include_audio = Some(include);
        self
    }

    /// Enable or disable the file cache.
    pub fn file_cache(mut self, enabled: bool) -> Self {
        self.download.file_cache = Some(enabled);
        self
    }

    /// Enable or disable the metadata cache.
    pub fn metadata_cache(mut self, enabled: bool) -> Self {
        self.download.metadata_cache = Some(enabled);
        self
    }

    /// Allow partial gallery results.
    pub fn allow_partial(mut self, allow: bool) -> Self {
        self.download.allow_partial = Some(allow);
        self
    }

    /// Enable or disable resumable video downloads.
    pub fn resume(mut self, enabled: bool) -> Self {
        self.download.resume = Some(enabled);
        self
    }

    /// Attach a batch progress reporter.
    pub fn progress(mut self, reporter: ProgressReporter) -> Self {
        self.progress = Some(reporter);
        self
    }

    /// Attach a cancellation token.
    pub fn signal(mut self, signal: CancellationToken) -> Self {
        self.signal = Some(signal);
        self
    }

    /// Attach a cancellation token to one item index.
    pub fn item_signal(mut self, index: usize, signal: CancellationToken) -> Self {
        self.item_signals.insert(index, signal);
        self
    }
}

/// Result for one URL in a batch.
#[derive(Debug, Clone, Serialize)]
pub struct BatchItemResult {
    /// Original URL, in input order.
    pub url: String,
    /// Successful download, if any.
    pub result: Option<DownloadResult>,
    /// Stable error payload, if the item failed.
    pub error: Option<ErrorInfo>,
}

/// Results for a batch, preserving input order.
#[derive(Debug, Clone, Serialize)]
pub struct BatchResult {
    /// Per-URL results in input order.
    pub items: Vec<BatchItemResult>,
    /// End-to-end wall time.
    pub total_ms: u64,
}

impl BatchResult {
    /// Number of successful items.
    pub fn successes(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.result.is_some())
            .count()
    }

    /// Number of failed items.
    pub fn failures(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.error.is_some())
            .count()
    }
}
