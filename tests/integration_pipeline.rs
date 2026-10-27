//! End-to-end pipeline tests against a local HTTP server.
//!
//! Not run by default because media URLs are HTTP on localhost. Run with:
//! `cargo test --features integration-tests --test integration_pipeline`.

#![cfg(feature = "integration-tests")]

mod support;

use std::sync::atomic::Ordering;

use ystt::{
    BatchOptions, CancellationToken, DownloadOptions, MediaKind, ProgressStage, Ystt, YsttError,
    progress_channel,
};

fn video_url() -> &'static str {
    "https://www.tiktok.com/@test/video/123"
}

fn gallery_url() -> &'static str {
    "https://www.tiktok.com/@test/video/456"
}

fn video_url_two() -> &'static str {
    "https://www.tiktok.com/@test/video/124"
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
        .metadata_cache(false)
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
        match event.stage {
            ProgressStage::MediaStarted { .. } => started = true,
            ProgressStage::MediaProgress {
                downloaded_bytes, ..
            } if downloaded_bytes > 0 => {
                progressed = true;
            }
            ProgressStage::MediaCompleted { .. } => completed = true,
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
async fn video_resume_appends_from_part_file() {
    let server = support::start().await;
    let dir = tempfile::tempdir().unwrap();
    let ystt = Ystt::builder()
        .output_dir(dir.path())
        .page_origin(server.origin.clone())
        .file_cache(false)
        .metadata_cache(false)
        .build()
        .unwrap();
    let path = dir.path().join("tiktok_test_123.mp4");
    let part = dir.path().join(".tiktok_test_123.mp4.part");
    let body = support::video_body();
    tokio::fs::write(&part, &body[..1000]).await.unwrap();

    let result = ystt
        .download_with(
            video_url(),
            DownloadOptions::new().output_dir(dir.path()).resume(true),
        )
        .await
        .unwrap();
    let video = result.video.expect("video");
    assert_eq!(video.size as usize, support::VIDEO_SIZE);
    assert_eq!(tokio::fs::read(&video.path).await.unwrap(), body);
    assert!(!part.exists());
    assert!(!path.with_file_name("tiktok_test_123.mp4.part").exists());
}

#[tokio::test]
async fn audio_failure_is_reported_and_video_is_not_cached() {
    let server = support::start().await;
    let dir = tempfile::tempdir().unwrap();
    server
        .counters
        .reject_audio
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let ystt = Ystt::builder()
        .output_dir(dir.path())
        .page_origin(server.origin.clone())
        .file_cache(true)
        .file_cache_dir(dir.path().join("cache"))
        .metadata_cache(false)
        .build()
        .unwrap();
    let error = ystt
        .download_with(
            video_url(),
            DownloadOptions::new()
                .output_dir(dir.path())
                .include_audio(true),
        )
        .await
        .expect_err("audio failure must fail the job");
    assert_eq!(error.status(), Some(403));
    assert!(!dir.path().join("cache").join("manifests").exists());
}

#[tokio::test]
async fn video_downloads_into_memory_without_touching_disk() {
    let server = support::start().await;
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("output");
    let ystt = Ystt::builder()
        .output_dir(&output)
        .page_origin(server.origin.clone())
        .file_cache(false)
        .metadata_cache(false)
        .build()
        .unwrap();
    let video = ystt
        .download_video_bytes_with(
            video_url(),
            DownloadOptions::new().max_bytes(16 * 1024 * 1024),
        )
        .await
        .unwrap();
    assert_eq!(video.post_id, "123");
    assert_eq!(video.mime, "video/mp4");
    assert!(video.bytes.len() > 1000);
    assert_eq!(&video.bytes[4..8], b"ftyp");
    assert!(
        !output.exists(),
        "in-memory download must not create output files"
    );
    assert_eq!(server.counters.media.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn video_bytes_use_parallel_ranges_for_large_payloads() {
    let server = support::start().await;
    let dir = tempfile::tempdir().unwrap();
    let ystt = Ystt::builder()
        .output_dir(dir.path())
        .page_origin(server.origin.clone())
        .file_cache(false)
        .metadata_cache(false)
        .parallel_ranges(true)
        .max_connections(4)
        .range_threshold(64 * 1024)
        .range_first_chunk(64 * 1024)
        .range_min_chunk(64 * 1024)
        .build()
        .unwrap();
    let video = ystt
        .download_video_bytes_with(
            video_url(),
            DownloadOptions::new().max_bytes(16 * 1024 * 1024),
        )
        .await
        .unwrap();
    assert_eq!(video.bytes.len(), support::VIDEO_SIZE);
    assert_eq!(&video.bytes[4..8], b"ftyp");
    assert!(
        server.counters.ranges.load(Ordering::Relaxed) >= 3,
        "parallel range path must issue follow-up range requests"
    );
    assert_eq!(server.counters.media.load(Ordering::Relaxed), 1 + 3);
}

#[tokio::test]
async fn filename_template_is_used() {
    let server = support::start().await;
    let dir = tempfile::tempdir().unwrap();
    let ystt = Ystt::builder()
        .output_dir(dir.path())
        .page_origin(server.origin.clone())
        .file_cache(false)
        .metadata_cache(false)
        .build()
        .unwrap();
    let result = ystt
        .download_with(
            video_url(),
            DownloadOptions::new()
                .output_dir(dir.path())
                .filename_template("{author}-{post_id}-{quality}"),
        )
        .await
        .unwrap();
    let name = result
        .video
        .unwrap()
        .path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(name, "test-123-balanced.mp4");
}

#[tokio::test]
async fn audio_downloads_through_the_same_pipeline() {
    let server = support::start().await;
    let dir = tempfile::tempdir().unwrap();
    let ystt = Ystt::builder()
        .output_dir(dir.path())
        .page_origin(server.origin.clone())
        .file_cache(false)
        .metadata_cache(false)
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
async fn metadata_cache_is_shared_between_instances() {
    let server = support::start().await;
    let dir = tempfile::tempdir().unwrap();
    let metadata = tempfile::tempdir().unwrap();

    let build = || {
        Ystt::builder()
            .output_dir(dir.path())
            .page_origin(server.origin.clone())
            .file_cache(false)
            .metadata_cache_dir(metadata.path())
            .metadata_cache_ttl(std::time::Duration::from_secs(60))
            .build()
            .unwrap()
    };

    let first = build()
        .download_with(video_url(), DownloadOptions::new().output_dir(dir.path()))
        .await
        .unwrap();
    assert!(first.video.is_some());
    assert_eq!(server.counters.pages.load(Ordering::Relaxed), 1);

    // A new process equivalent: no in-memory cache, same persistent metadata.
    let second = build()
        .download_with(video_url(), DownloadOptions::new().output_dir(dir.path()))
        .await
        .unwrap();
    assert!(second.video.is_some());
    assert_eq!(
        server.counters.pages.load(Ordering::Relaxed),
        1,
        "metadata hit must skip the page request"
    );
    assert_eq!(server.counters.media.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn shared_file_cache_works_across_output_directories() {
    let server = support::start().await;
    let cache = tempfile::tempdir().unwrap();
    let first_dir = tempfile::tempdir().unwrap();
    let second_dir = tempfile::tempdir().unwrap();

    let first = Ystt::builder()
        .output_dir(first_dir.path())
        .page_origin(server.origin.clone())
        .cache_dir(cache.path())
        .metadata_cache(false)
        .build()
        .unwrap()
        .download_with(
            video_url(),
            DownloadOptions::new().output_dir(first_dir.path()),
        )
        .await
        .unwrap();
    assert!(!first.cache_hit);

    let second = Ystt::builder()
        .output_dir(second_dir.path())
        .page_origin(server.origin.clone())
        .cache_dir(cache.path())
        .metadata_cache(false)
        .build()
        .unwrap()
        .download_with(
            video_url(),
            DownloadOptions::new().output_dir(second_dir.path()),
        )
        .await
        .unwrap();

    assert!(second.cache_hit);
    assert_eq!(server.counters.pages.load(Ordering::Relaxed), 1);
    assert_eq!(server.counters.media.load(Ordering::Relaxed), 1);
    assert!(second.video.unwrap().path.starts_with(second_dir.path()));
}

#[tokio::test]
async fn batch_preserves_order_and_reports_errors() {
    let server = support::start().await;
    let dir = tempfile::tempdir().unwrap();
    let ystt = Ystt::builder()
        .output_dir(dir.path())
        .page_origin(server.origin.clone())
        .file_cache(false)
        .metadata_cache(false)
        .build()
        .unwrap();

    let urls = [
        video_url(),
        "https://www.tiktok.com/@test/video/999",
        video_url_two(),
    ];
    let (reporter, mut events) = progress_channel();
    let batch = ystt
        .download_many(
            urls,
            BatchOptions::new()
                .output_dir(dir.path())
                .concurrency(2)
                .progress(reporter),
        )
        .await;

    let mut started = false;
    let mut progressed = false;
    let mut completed = false;
    let mut item_event = false;
    while let Ok(event) = events.try_recv() {
        match event.stage {
            ProgressStage::BatchStarted { total_items } => {
                assert_eq!(total_items, 3);
                started = true;
            }
            ProgressStage::BatchProgress {
                completed_items,
                failed_items,
                total_items,
            } => {
                assert_eq!(total_items, 3);
                assert!(completed_items + failed_items > 0);
                progressed = true;
            }
            ProgressStage::BatchCompleted {
                successes,
                failures,
                ..
            } => {
                assert_eq!(successes, 2);
                assert_eq!(failures, 1);
                completed = true;
            }
            _ if event.batch_index.is_some() => item_event = true,
            _ => {}
        }
    }
    assert!(started && progressed && completed && item_event);

    assert_eq!(batch.items.len(), 3);
    assert_eq!(batch.successes(), 2);
    assert_eq!(batch.failures(), 1);
    assert_eq!(batch.items[0].url, video_url());
    assert!(batch.items[0].result.is_some());
    assert!(batch.items[1].result.is_none());
    assert_eq!(
        batch.items[1].error.as_ref().map(|error| error.code),
        Some("unavailable")
    );
    assert_eq!(batch.items[2].url, video_url_two());
}

#[tokio::test]
async fn concurrent_downloads_share_one_file_cache_lock() {
    let server = support::start().await;
    let cache = tempfile::tempdir().unwrap();
    let first_dir = tempfile::tempdir().unwrap();
    let second_dir = tempfile::tempdir().unwrap();
    let build = |dir: &std::path::Path| {
        Ystt::builder()
            .output_dir(dir)
            .page_origin(server.origin.clone())
            .cache_dir(cache.path())
            .metadata_cache(false)
            .build()
            .unwrap()
    };
    let first = build(first_dir.path());
    let second = build(second_dir.path());
    let (a, b) = tokio::join!(
        first.download_with(
            video_url(),
            DownloadOptions::new().output_dir(first_dir.path())
        ),
        second.download_with(
            video_url(),
            DownloadOptions::new().output_dir(second_dir.path())
        )
    );
    assert!(a.is_ok());
    assert!(b.is_ok());
    assert_eq!(
        server.counters.pages.load(Ordering::Relaxed),
        1,
        "the lock must prevent duplicate page work"
    );
    assert_eq!(
        server.counters.media.load(Ordering::Relaxed),
        1,
        "the lock must prevent duplicate media work"
    );
    assert!(a.unwrap().cache_hit || b.unwrap().cache_hit);
}

#[tokio::test]
async fn media_403_re_resolves_cached_metadata_once() {
    let server = support::start().await;
    let dir = tempfile::tempdir().unwrap();
    let ystt = Ystt::builder()
        .output_dir(dir.path())
        .page_origin(server.origin.clone())
        .file_cache(false)
        .metadata_cache(false)
        .build()
        .unwrap();
    ystt.download_with(video_url(), DownloadOptions::new().output_dir(dir.path()))
        .await
        .unwrap();
    assert_eq!(server.counters.pages.load(Ordering::Relaxed), 1);
    server
        .counters
        .reject_media
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let error = ystt
        .download_with(video_url(), DownloadOptions::new().output_dir(dir.path()))
        .await
        .expect_err("media must fail");
    assert_eq!(error.status(), Some(403));
    assert_eq!(
        server.counters.pages.load(Ordering::Relaxed),
        2,
        "a cached URL 403 must trigger exactly one full-page re-resolve"
    );
}

#[tokio::test]
async fn batch_item_cancellation_does_not_cancel_other_items() {
    let server = support::start().await;
    let dir = tempfile::tempdir().unwrap();
    let ystt = Ystt::builder()
        .output_dir(dir.path())
        .page_origin(server.origin.clone())
        .file_cache(false)
        .metadata_cache(false)
        .build()
        .unwrap();
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let batch = ystt
        .download_many(
            [video_url(), video_url_two()],
            BatchOptions::new()
                .output_dir(dir.path())
                .item_signal(1, cancelled),
        )
        .await;
    assert!(batch.items[0].result.is_some());
    assert_eq!(
        batch.items[1].error.as_ref().map(|error| error.code),
        Some("cancelled")
    );
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
        .metadata_cache(false)
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
        .metadata_cache(false)
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
