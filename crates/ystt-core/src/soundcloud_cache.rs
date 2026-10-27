//! Opt-in disk cache for SoundCloud audio.
//!
//! The API and Telegram bot intentionally keep media in RAM. This cache exists
//! for the CLI and for library consumers that explicitly want repeat SoundCloud
//! downloads to skip the network. Objects are immutable, manifests are
//! atomically replaced, and expired entries are ignored.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::fs;
use tracing::warn;

use crate::cache_util::{acquire_lock, temp_sibling};
use crate::config::QualityPolicy;
use crate::error::{Result, YsttError};
use crate::soundcloud::{SoundCloudArtwork, SoundCloudAudio, SoundCloudTrack};

#[derive(Debug, Serialize, Deserialize)]
struct CacheEntry {
    version: u8,
    track: SoundCloudTrack,
    quality: String,
    mime: String,
    ext: String,
    source_url: String,
    preset: Option<String>,
    audio_file: String,
    size: u64,
    artwork_file: Option<String>,
    artwork_mime: Option<String>,
    artwork_url: Option<String>,
    created_unix: u64,
}

/// Content-addressed-ish SoundCloud disk cache keyed by track ID + quality.
#[derive(Debug, Clone)]
pub struct SoundCloudCache {
    root: PathBuf,
    ttl: Duration,
}

impl SoundCloudCache {
    /// Create a cache rooted at `root`.
    pub fn new(root: impl Into<PathBuf>, ttl: Duration) -> Self {
        Self {
            root: root.into(),
            ttl,
        }
    }

    /// Cache root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Read a live cached track.
    pub async fn get(&self, track_id: &str, quality: QualityPolicy) -> Option<SoundCloudAudio> {
        let key = cache_key(track_id, quality);
        let manifest = self.root.join("manifests").join(format!("{key}.json"));
        let bytes = fs::read(&manifest).await.ok()?;
        let entry: CacheEntry = match serde_json::from_slice(&bytes) {
            Ok(entry) => entry,
            Err(error) => {
                warn!(?error, path = %manifest.display(), "invalid SoundCloud cache manifest");
                let _ = fs::remove_file(&manifest).await;
                return None;
            }
        };
        if entry.version != 1 || expired(entry.created_unix, self.ttl) {
            let _ = fs::remove_file(&manifest).await;
            return None;
        }
        let audio_path = self.root.join("objects").join(&entry.audio_file);
        let bytes = fs::read(&audio_path).await.ok()?;
        if bytes.len() as u64 != entry.size {
            let _ = fs::remove_file(&manifest).await;
            return None;
        }
        let artwork = match (&entry.artwork_file, &entry.artwork_mime, &entry.artwork_url) {
            (Some(file), Some(mime), Some(url)) => {
                let artwork_bytes = fs::read(self.root.join("objects").join(file)).await.ok()?;
                Some(SoundCloudArtwork {
                    bytes: artwork_bytes.into(),
                    mime: mime.clone(),
                    url: url.clone(),
                })
            }
            _ => None,
        };
        Some(SoundCloudAudio {
            track: entry.track,
            bytes: Bytes::from(bytes),
            mime: entry.mime,
            ext: entry.ext,
            source_url: entry.source_url,
            preset: entry.preset,
            artwork,
        })
    }

    /// Store a downloaded track. Cache failures are surfaced to the caller so
    /// the CLI can log them without failing the actual download.
    pub async fn put(&self, audio: &SoundCloudAudio, quality: QualityPolicy) -> Result<()> {
        let key = cache_key(&audio.track.id, quality);
        let _lock = acquire_lock(&self.root.join("locks").join(format!("{key}.lock"))).await?;
        let objects = self.root.join("objects");
        let manifests = self.root.join("manifests");
        fs::create_dir_all(&objects).await?;
        fs::create_dir_all(&manifests).await?;

        let audio_file = format!("{key}.{}", audio.ext);
        let audio_path = objects.join(&audio_file);
        if fs::metadata(&audio_path)
            .await
            .map(|metadata| metadata.len() == audio.bytes.len() as u64)
            .unwrap_or(false)
        {
            // Existing identical object; keep it.
        } else {
            atomic_write(&audio_path, &audio.bytes).await?;
        }

        let (artwork_file, artwork_mime, artwork_url) = match &audio.artwork {
            Some(artwork) => {
                let file = format!("{key}-cover.jpg");
                atomic_write(&objects.join(&file), &artwork.bytes).await?;
                (
                    Some(file),
                    Some(artwork.mime.clone()),
                    Some(artwork.url.clone()),
                )
            }
            None => (None, None, None),
        };

        let entry = CacheEntry {
            version: 1,
            track: audio.track.clone(),
            quality: quality.as_str().to_owned(),
            mime: audio.mime.clone(),
            ext: audio.ext.clone(),
            source_url: audio.source_url.clone(),
            preset: audio.preset.clone(),
            audio_file,
            size: audio.bytes.len() as u64,
            artwork_file,
            artwork_mime,
            artwork_url,
            created_unix: unix_now(),
        };
        let manifest = manifests.join(format!("{key}.json"));
        atomic_write(&manifest, &serde_json::to_vec(&entry)?).await?;
        Ok(())
    }

    /// Remove expired manifests and unreferenced object files.
    pub async fn prune(&self) -> Result<usize> {
        let manifests = self.root.join("manifests");
        let objects = self.root.join("objects");
        let mut referenced = std::collections::HashSet::new();
        let mut removed = 0usize;
        if let Ok(mut entries) = fs::read_dir(&manifests).await {
            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();
                let Ok(bytes) = fs::read(&path).await else {
                    continue;
                };
                let Ok(entry) = serde_json::from_slice::<CacheEntry>(&bytes) else {
                    let _ = fs::remove_file(&path).await;
                    continue;
                };
                if expired(entry.created_unix, self.ttl) {
                    let _ = fs::remove_file(&path).await;
                    continue;
                }
                referenced.insert(objects.join(entry.audio_file));
                if let Some(file) = entry.artwork_file {
                    referenced.insert(objects.join(file));
                }
            }
        }
        if let Ok(mut entries) = fs::read_dir(&objects).await {
            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();
                if !referenced.contains(&path) && fs::remove_file(&path).await.is_ok() {
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }
}

fn cache_key(track_id: &str, quality: QualityPolicy) -> String {
    let mut safe = String::with_capacity(track_id.len() + 8);
    for character in track_id.chars() {
        if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
            safe.push(character);
        } else {
            safe.push('_');
        }
    }
    format!("{safe}-{}", quality.as_str())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn expired(created_unix: u64, ttl: Duration) -> bool {
    let now = unix_now();
    now.saturating_sub(created_unix) >= ttl.as_secs()
}

async fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| YsttError::Internal("cache path has no parent".into()))?;
    fs::create_dir_all(parent).await?;
    let temp = temp_sibling(path);
    fs::write(&temp, bytes).await?;
    fs::rename(&temp, path).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::soundcloud::SoundCloudArtwork;

    fn track() -> SoundCloudTrack {
        serde_json::from_value(serde_json::json!({
            "id": "1",
            "title": "Track",
            "artist": "Artist",
            "artist_id": "2",
            "duration_ms": 1000,
            "permalink_url": "https://soundcloud.com/a/track",
            "artwork_url": "https://i1.sndcdn.com/a-t300x300.jpg",
            "policy": "MONETIZE",
            "description": null,
            "genre": null,
            "transcodings": []
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn roundtrip_and_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let cache = SoundCloudCache::new(dir.path(), Duration::from_secs(60));
        let audio = SoundCloudAudio {
            track: track(),
            bytes: Bytes::from_static(b"audio"),
            mime: "audio/mp4".into(),
            ext: "m4a".into(),
            source_url: "https://cdn.test/a".into(),
            preset: Some("aac_160k".into()),
            artwork: Some(SoundCloudArtwork {
                bytes: Bytes::from_static(b"jpeg"),
                mime: "image/jpeg".into(),
                url: "https://i1.sndcdn.com/a-t300x300.jpg".into(),
            }),
        };
        cache.put(&audio, QualityPolicy::Balanced).await.unwrap();
        let hit = cache.get("1", QualityPolicy::Balanced).await.unwrap();
        assert_eq!(hit.bytes, audio.bytes);
        assert_eq!(hit.artwork.unwrap().bytes, Bytes::from_static(b"jpeg"));
        assert!(cache.get("2", QualityPolicy::Balanced).await.is_none());

        let expired = SoundCloudCache::new(dir.path(), Duration::from_secs(0));
        assert!(expired.get("1", QualityPolicy::Balanced).await.is_none());
    }
}
