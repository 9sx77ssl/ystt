//! End-to-end pipeline tests against a local HTTP server.
//!
//! Not run by default because media URLs are HTTP on localhost. Run with:
//! `cargo test --features integration-tests --test integration_pipeline`.

#![cfg(feature = "integration-tests")]

mod support;

use std::sync::atomic::Ordering;

use ystt::{DownloadOptions, MediaKind, ProgressEvent, Ystt, YsttError, progress_channel};

fn video_url() -> &'static str {
    "https://www.tiktok.com/@test/video/123"
}

fn gallery_url() -> &'static str {
    "https://www.tiktok.com/@test/video/456"
}

async fn build(server: &support::TestServer, dir: &std::path::Path) -> Ystt {
    Ystt::builder()
        .output_dir(dir)
        .page_origin(server.origin.clone())
        .parallel_ranges(true)
        .range_first_chunk(64 * 1024)
        .range_min_chunk(32 * 1024)
        .range_threshold(0)
        .max_connections(4)
        .file_cache(true)
        .file_cache_dir(dir.join("cache"))
        .build()
        .unwrap()
}

#[tokio::test]
async fn video_ranges_cookies_progress_and_file_cache() {
    let server = support::start().await;
    let dir = tempfile::tempdir().unwrap();
    let ystt = build(&server, dir.path()).await;

    let (reporter, mut events) = progress_channel();
    let result = ystt
        .download_with(
            video_url(),
            DownloadOptions::new()
                .output_dir(dir.path())
                .progress(reporter),
        )
        .await
        .unwrap();

    let video = result.video.expect("video");
    assert!(!result.cache_hit);
    assert!(result.failures.is_empty());
    assert_eq!(video.size as usize, support::VIDEO_SIZE);
    let bytes = tokio::fs::read(&video.path).await.unwrap();
    assert_eq!(bytes.len(), support::VIDEO_SIZE);
    assert_eq!(&bytes[4..8], b"ftyp");

    assert!(server.counters.media.load(Ordering::Relaxed) >= 1);
    assert!(
        server.counters.ranges.load(Ordering::Relaxed) >= 2,
        "expected multiple ranges (total={}, media={}, ranges={})",
        server.counters.total.load(Ordering::Relaxed),
        server.counters.media.load(Ordering::Relaxed),
        server.counters.ranges.load(Ordering::Relaxed)
    );

    let mut started = false;
    let mut progressed = false;
    let mut completed = false;
    while let Ok(event) = events.try_recv() {
        match event {
            ProgressEvent::MediaStarted { .. } => started = true,
            ProgressEvent::MediaProgress {
                downloaded_bytes, ..
            } if downloaded_bytes > 0 => {
                progressed = true;
            }
            ProgressEvent::MediaCompleted { .. } => completed = true,
            _ => {}
        }
    }
    assert!(started && progressed && completed);

    let before = server.counters.total.load(Ordering::Relaxed);
    let cached = ystt
        .download_with(video_url(), DownloadOptions::new().output_dir(dir.path()))
        .await
        .unwrap();
    assert!(cached.cache_hit);
    assert_eq!(server.counters.total.load(Ordering::Relaxed), before);

    // Simulate a process restart: a new client has no URL cache, but the
    // content-addressed file cache still answers without any request.
    let ystt_restarted = build(&server, dir.path()).await;
    let before = server.counters.total.load(Ordering::Relaxed);
    let cached = ystt_restarted
        .download_with(video_url(), DownloadOptions::new().output_dir(dir.path()))
        .await
        .unwrap();
    assert!(cached.cache_hit);
    assert_eq!(server.counters.total.load(Ordering::Relaxed), before);
}

#[tokio::test]
async fn audio_downloads_through_the_same_pipeline() {
    let server = support::start().await;
    let dir = tempfile::tempdir().unwrap();
    let ystt = Ystt::builder()
        .output_dir(dir.path())
        .page_origin(server.origin.clone())
        .file_cache(false)
        .build()
        .unwrap();

    let result = ystt
        .download_with(
            video_url(),
            DownloadOptions::new()
                .output_dir(dir.path())
                .include_audio(true),
        )
        .await
        .unwrap();

    let video = result.video.expect("video");
    let audio = result.audio.expect("audio");
    assert_eq!(video.size as usize, support::VIDEO_SIZE);
    let bytes = tokio::fs::read(&audio.path).await.unwrap();
    assert!(bytes.starts_with(b"ID3"));
    assert!(result.failures.is_empty());
}

#[tokio::test]
async fn embed_resolver_can_be_forced() {
    let server = support::start().await;
    let dir = tempfile::tempdir().unwrap();
    let ystt = Ystt::builder()
        .output_dir(dir.path())
        .page_origin(server.origin.clone())
        .use_embed(true)
        .file_cache(false)
        .build()
        .unwrap();

    let result = ystt
        .download_with(video_url(), DownloadOptions::new().output_dir(dir.path()))
        .await
        .unwrap();
    assert_eq!(
        result.video.expect("video").size as usize,
        support::VIDEO_SIZE
    );
}

#[tokio::test]
async fn full_page_fallback_uses_session_cookies() {
    let server = support::start().await;
    let dir = tempfile::tempdir().unwrap();
    let ystt = Ystt::builder()
        .output_dir(dir.path())
        .page_origin(server.origin.clone())
        .use_embed(false)
        .file_cache(false)
        .build()
        .unwrap();

    let result = ystt
        .download_with(video_url(), DownloadOptions::new().output_dir(dir.path()))
        .await
        .unwrap();
    let video = result.video.expect("video");
    assert_eq!(video.size as usize, support::VIDEO_SIZE);
    assert!(server.counters.media.load(Ordering::Relaxed) >= 1);
}

#[tokio::test]
async fn gallery_partial_results_and_strict_mode() {
    let server = support::start().await;
    let dir = tempfile::tempdir().unwrap();
    let ystt = build(&server, dir.path()).await;

    let result = ystt
        .download_with(gallery_url(), DownloadOptions::new().output_dir(dir.path()))
        .await
        .unwrap();
    assert_eq!(result.photos.len(), 2);
    assert_eq!(result.failures.len(), 1);
    assert_eq!(result.failures[0].kind, MediaKind::Photo);
    for photo in &result.photos {
        let bytes = tokio::fs::read(&photo.path).await.unwrap();
        assert_eq!(&bytes[..3], &[0xFF, 0xD8, 0xFF]);
    }

    let error = ystt
        .download_with(
            gallery_url(),
            DownloadOptions::new()
                .output_dir(dir.path())
                .allow_partial(false)
                .file_cache(false),
        )
        .await
        .expect_err("strict mode must fail");
    assert!(matches!(error, YsttError::Partial(_)));
}
