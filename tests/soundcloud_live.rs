//! Opt-in real-network SoundCloud smoke tests.
//!
//! Run with `cargo test --test soundcloud_live -- --ignored --nocapture`.
//! They only touch public tracks and a public playlist; no cookies or accounts.

use ystt::{DownloadOptions, QualityPolicy, SoundCloudEntity, Ystt};

const TRACK: &str = "https://soundcloud.com/treezymonster812/rozovoe-vino-2-ft-morgenshtern";
const PLAYLIST: &str = "https://soundcloud.com/kaiangel-9mice/sets/shh-1";
const SNIPPED: &str = "https://soundcloud.com/marilynmanson/mechanical-animals";

#[tokio::test]
#[ignore = "real SoundCloud network"]
async fn real_track_downloads_valid_fmp4() {
    let ystt = Ystt::builder()
        .quality(QualityPolicy::Best)
        .file_cache(false)
        .metadata_cache(false)
        .build()
        .unwrap();
    let entity = ystt.resolve_soundcloud(TRACK).await.unwrap();
    match entity {
        SoundCloudEntity::Track(track) => {
            assert!(!track.title.is_empty());
            assert!(!track.transcodings.is_empty());
            assert!(
                track
                    .artwork_url
                    .as_deref()
                    .unwrap_or("")
                    .contains("sndcdn.com")
            );
        }
        SoundCloudEntity::Playlist(_) => panic!("expected track"),
    }
    let audio = ystt
        .download_soundcloud_track_with(TRACK, DownloadOptions::new().max_bytes(64 * 1024 * 1024))
        .await
        .unwrap();
    assert!(audio.bytes.len() > 100_000);
    assert!(audio.mime.starts_with("audio/"));
    assert!(matches!(audio.ext.as_str(), "m4a" | "mp3" | "ogg"));
    if audio.ext == "m4a" {
        assert!(audio.bytes.len() > 12);
        assert_eq!(&audio.bytes[4..8], b"ftyp");
    }
}

#[tokio::test]
#[ignore = "real SoundCloud network"]
async fn real_playlist_hydrates_all_tracks() {
    let ystt = Ystt::builder().build().unwrap();
    let entity = ystt.resolve_soundcloud(PLAYLIST).await.unwrap();
    match entity {
        SoundCloudEntity::Playlist(playlist) => {
            assert!(playlist.tracks.len() >= 10);
            assert!(playlist.tracks.iter().all(|track| !track.title.is_empty()));
            assert!(
                playlist
                    .tracks
                    .iter()
                    .filter(|track| !track.transcodings.is_empty())
                    .count()
                    >= 10
            );
        }
        SoundCloudEntity::Track(_) => panic!("expected playlist"),
    }
}

#[tokio::test]
#[ignore = "real SoundCloud network"]
async fn real_snipped_track_is_rejected() {
    let ystt = Ystt::builder().build().unwrap();
    let error = ystt.resolve_soundcloud(SNIPPED).await.unwrap_err();
    assert_eq!(error.code(), "unavailable");
    assert!(error.to_string().contains("policy"));
}
