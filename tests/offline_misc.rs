//! Offline tests for small public helpers.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;
use ystt::{DownloadOptions, MediaKind, QualityPolicy, media::sanitize_filename, signed_url_ttl};

#[test]
fn filenames_are_safe_on_every_platform() {
    assert_eq!(sanitize_filename("a/b:c*?"), "a_b_c__");
    assert_eq!(sanitize_filename(".."), "tiktok");
    assert_eq!(sanitize_filename("CON"), "_CON");
    assert_eq!(sanitize_filename("normal-name_123"), "normal-name_123");
    assert!(sanitize_filename(&"x".repeat(400)).len() <= 180);
}

#[test]
fn signed_url_ttl_is_clamped() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let url = format!("https://cdn.example/v.mp4?expire={}", now + 600);
    let ttl = signed_url_ttl(&url, Duration::from_secs(60));
    assert!(ttl >= Duration::from_secs(540));
    assert!(ttl <= Duration::from_secs(600));

    assert_eq!(
        signed_url_ttl("https://cdn.example/v.mp4", Duration::from_secs(42)),
        Duration::from_secs(42)
    );
}

#[test]
fn serde_names_are_stable() {
    assert_eq!(
        serde_json::to_value(MediaKind::Video).unwrap(),
        json!("video")
    );
    assert_eq!(
        serde_json::to_value(MediaKind::Photo).unwrap(),
        json!("photo")
    );
    assert_eq!(
        serde_json::to_value(MediaKind::Audio).unwrap(),
        json!("audio")
    );
    assert_eq!(
        serde_json::to_value(QualityPolicy::Compatible).unwrap(),
        json!("compatible")
    );
}

#[test]
fn download_option_builder_is_typed() {
    let options = DownloadOptions::new()
        .quality(QualityPolicy::Fast)
        .max_connections(4)
        .include_audio(true)
        .reuse_existing(true);
    assert_eq!(options.quality, Some(QualityPolicy::Fast));
    assert_eq!(options.max_connections, Some(4));
    assert_eq!(options.include_audio, Some(true));
    assert_eq!(options.reuse_existing, Some(true));
}
