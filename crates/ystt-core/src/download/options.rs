//! One fully resolved option set for a single download job.

use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

use crate::config::{DownloadOptions, QualityPolicy, YsttConfig};
use crate::progress::ProgressReporter;

/// Effective values after merging [`YsttConfig`] with [`DownloadOptions`].
#[derive(Clone)]
pub(crate) struct ResolvedOptions {
    pub(crate) output_dir: PathBuf,
    pub(crate) quality: QualityPolicy,
    pub(crate) filename_template: Option<String>,
    pub(crate) include_audio: bool,
    pub(crate) allow_partial: bool,
    pub(crate) reuse_existing: bool,
    pub(crate) parallel_ranges: bool,
    pub(crate) max_connections: usize,
    pub(crate) photo_concurrency: usize,
    pub(crate) max_bytes: u64,
    pub(crate) media_retries: u32,
    pub(crate) resume: bool,
    pub(crate) adaptive_ranges: bool,
    pub(crate) range_start_connections: usize,
    pub(crate) range_first_chunk: u64,
    pub(crate) range_min_chunk: u64,
    pub(crate) range_threshold: u64,
    pub(crate) file_cache: bool,
    pub(crate) metadata_cache: bool,
    pub(crate) progress: Option<ProgressReporter>,
    pub(crate) signal: Option<CancellationToken>,
}

impl ResolvedOptions {
    pub(crate) fn resolve(config: &YsttConfig, options: &DownloadOptions) -> Self {
        Self {
            output_dir: options
                .output_dir
                .clone()
                .unwrap_or_else(|| config.output.dir.clone()),
            quality: options.quality.unwrap_or(config.output.quality),
            filename_template: options
                .filename_template
                .clone()
                .or_else(|| config.output.filename_template.clone()),
            include_audio: options.include_audio.unwrap_or(config.output.include_audio),
            allow_partial: options.allow_partial.unwrap_or(config.output.allow_partial),
            reuse_existing: options
                .reuse_existing
                .unwrap_or(config.output.reuse_existing),
            parallel_ranges: options
                .parallel_ranges
                .unwrap_or(config.media.parallel_ranges),
            max_connections: options
                .max_connections
                .unwrap_or(config.media.max_connections)
                .max(1),
            photo_concurrency: options
                .photo_concurrency
                .unwrap_or(config.media.photo_concurrency)
                .max(1),
            max_bytes: options.max_bytes.unwrap_or(config.media.max_bytes),
            media_retries: options.media_retries.unwrap_or(config.media.retries),
            resume: options.resume.unwrap_or(config.media.resume),
            adaptive_ranges: options
                .adaptive_ranges
                .unwrap_or(config.media.adaptive_ranges),
            range_start_connections: config.media.range_start_connections,
            range_first_chunk: config.media.range_first_chunk,
            range_min_chunk: config.media.range_min_chunk,
            range_threshold: config.media.range_threshold,
            file_cache: options.file_cache.unwrap_or(config.cache.file),
            metadata_cache: options.metadata_cache.unwrap_or(config.cache.metadata),
            progress: options.progress.clone(),
            signal: options.signal.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_are_applied_once() {
        let config = YsttConfig::default();
        let resolved = ResolvedOptions::resolve(&config, &DownloadOptions::new());
        assert_eq!(resolved.output_dir, config.output.dir);
        assert_eq!(resolved.quality, config.output.quality);
        assert_eq!(resolved.max_connections, config.media.max_connections);
        assert_eq!(resolved.media_retries, config.media.retries);
        assert_eq!(resolved.photo_concurrency, config.media.photo_concurrency);
        assert_eq!(resolved.range_threshold, config.media.range_threshold);
        assert_eq!(resolved.file_cache, config.cache.file);
        assert_eq!(resolved.metadata_cache, config.cache.metadata);
    }

    #[test]
    fn per_call_overrides_win() {
        let config = YsttConfig::default();
        let options = DownloadOptions::new()
            .output_dir("custom")
            .quality(QualityPolicy::Best)
            .max_connections(12)
            .media_retries(7)
            .resume(true)
            .file_cache(false)
            .metadata_cache(false)
            .allow_partial(false);
        let resolved = ResolvedOptions::resolve(&config, &options);
        assert_eq!(resolved.output_dir, PathBuf::from("custom"));
        assert_eq!(resolved.quality, QualityPolicy::Best);
        assert_eq!(resolved.max_connections, 12);
        assert_eq!(resolved.media_retries, 7);
        assert!(resolved.resume);
        assert!(!resolved.file_cache);
        assert!(!resolved.metadata_cache);
        assert!(!resolved.allow_partial);
    }
}
