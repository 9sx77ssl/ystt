//! Media file downloading: single stream, range-first parallel, validation.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio_util::sync::CancellationToken;
use tracing::warn;
use ystt_net::HttpClient;

use crate::error::{Result, YsttError};
mod ext;
mod transfer;
mod validation;

pub use ext::sanitize_filename;
pub(crate) use ext::{audio_ext, base_filename, mime_for_ext, photo_ext, video_ext};
#[cfg(test)]
pub(crate) use transfer::parse_content_range;
pub(crate) use transfer::{download_adaptive, download_resume, download_single, should_fallback};
#[cfg(test)]
pub(crate) use validation::validate_download;
#[cfg(test)]
pub(crate) use validation::{is_jpeg, is_mp4, is_png, is_webm, is_webp};

use crate::model::{MediaKind, is_https_url};
use crate::progress::{MediaTarget, ProgressEmitter, ProgressTracker};

/// One downloaded media file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadedFile {
    /// Absolute or caller-supplied path.
    pub path: PathBuf,
    /// MIME type.
    pub mime: String,
    /// Extension without a dot.
    pub ext: String,
    /// Size in bytes.
    pub size: u64,
    /// Source CDN URL.
    pub url: String,
    /// Media kind.
    pub kind: MediaKind,
}

/// Shared context for one media transfer.
#[derive(Clone)]
pub(crate) struct MediaContext<'a> {
    pub client: &'a HttpClient,
    pub referer: &'a str,
    pub cookie: Option<&'a str>,
    pub signal: Option<&'a CancellationToken>,
    pub max_bytes: u64,
    pub read_timeout: Duration,
    pub parallel_ranges: bool,
    pub max_connections: usize,
    pub first_chunk: u64,
    pub min_chunk: u64,
    pub range_threshold: u64,
    pub reuse_existing: bool,
    pub resume: bool,
    pub media_retries: u32,
    pub progress: Option<ProgressEmitter>,
    pub adaptive_ranges: bool,
    pub range_start_connections: usize,
}

/// Download one media resource into `path`.
pub(crate) async fn download_media(
    context: &MediaContext<'_>,
    url: &str,
    path: &Path,
    kind: MediaKind,
    target: MediaTarget,
) -> Result<DownloadedFile> {
    if !is_https_url(url) {
        return Err(YsttError::Parse(format!(
            "media URL is not absolute HTTPS: {url}"
        )));
    }

    if context.reuse_existing
        && let Ok(metadata) = fs::metadata(path).await
        && metadata.is_file()
        && metadata.len() > 0
    {
        return DownloadedFile::from_path(path, url, kind, metadata.len());
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }

    let tracker = context
        .progress
        .clone()
        .map(|emitter| ProgressTracker::new(emitter, target, None));

    let size = match kind {
        MediaKind::Video if context.resume => {
            download_resume(context, url, path, target, tracker.clone()).await?
        }
        MediaKind::Video if context.parallel_ranges => {
            match download_adaptive(context, url, path, tracker.clone()).await {
                Ok(size) => size,
                Err(error) if should_fallback(&error) => {
                    warn!(?error, "range download failed; falling back to one stream");
                    download_single(context, url, path, target, tracker.clone()).await?
                }
                Err(error) => return Err(error),
            }
        }
        _ => download_single(context, url, path, target, tracker.clone()).await?,
    };

    if let Some(tracker) = &tracker {
        tracker.complete();
    }
    DownloadedFile::from_path(path, url, kind, size)
}

impl DownloadedFile {
    fn from_path(path: &Path, url: &str, kind: MediaKind, size: u64) -> Result<Self> {
        let ext = path
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("bin")
            .to_ascii_lowercase();
        let mime = mime_for_ext(&ext, kind);
        Ok(Self {
            path: path.to_path_buf(),
            mime,
            ext,
            size,
            url: url.to_owned(),
            kind,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_range_is_parsed() {
        assert_eq!(
            parse_content_range("bytes 0-1023/2048"),
            Some((0, 1023, Some(2048)))
        );
        assert_eq!(
            parse_content_range("bytes 1024-2047/*"),
            Some((1024, 2047, None))
        );
        assert_eq!(parse_content_range("garbage"), None);
    }

    #[test]
    fn extensions_are_normalized() {
        assert_eq!(video_ext("https://cdn/video.mp4?x=1"), "mp4");
        assert_eq!(video_ext("https://cdn/video.unknown?x=1"), "mp4");
        assert_eq!(photo_ext("https://cdn/p.jpeg?x=1"), "jpg");
        assert_eq!(photo_ext("https://cdn/p.webp"), "webp");
        assert_eq!(audio_ext("https://cdn/a.m4a?x=1"), "m4a");
    }

    #[test]
    fn filenames_are_safe() {
        assert_eq!(sanitize_filename("a/b:c*?"), "a_b_c__");
        assert_eq!(sanitize_filename(".."), "tiktok");
        assert_eq!(sanitize_filename("CON"), "_CON");
        assert!(sanitize_filename(&"x".repeat(400)).len() <= 180);
    }

    #[test]
    fn filename_template_renders_and_sanitizes() {
        assert_eq!(
            base_filename(Some("{author}-{post_id}-{quality}"), "u", "123", "best"),
            "u-123-best"
        );
        assert_eq!(
            base_filename(Some("../../{post_id}"), "u", "123", "best"),
            "_.._123"
        );
        assert_eq!(base_filename(None, "u", "123", "balanced"), "tiktok_u_123");
    }

    #[test]
    fn signatures_are_checked() {
        assert!(is_mp4(b"\0\0\0\x20ftypisom"));
        assert!(is_webm(&[0x1A, 0x45, 0xDF, 0xA3]));
        assert!(is_jpeg(&[0xFF, 0xD8, 0xFF, 0xE0]));
        assert!(is_png(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]));
        assert!(is_webp(b"RIFF\0\0\0\0WEBP"));
        assert!(!is_jpeg(b"<html>"));
    }

    #[tokio::test]
    async fn html_is_rejected_as_media() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.mp4");
        tokio::fs::write(&path, b"<html>blocked</html>")
            .await
            .unwrap();
        assert!(matches!(
            validate_download(&path, MediaKind::Video).await,
            Err(YsttError::Parse(_))
        ));
    }
}
