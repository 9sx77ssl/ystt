//! Persistent metadata cache for resolved signed URLs.
//!
//! The cache is intentionally small and separate from the content-addressed
//! media cache. Each entry stores the resolved [`MediaInfo`] plus only the
//! server-set session cookies needed by TikTok's media CDN. Account cookies are
//! never accepted or stored.
//!
//! Entries are JSON files with an explicit schema version and an absolute
//! expiry derived from the signed URL. Writes are atomic and, on Unix, the
//! files are restricted to the owner.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::fs;
use tracing::warn;

use crate::cache::media_ttl;
use crate::cache_util::{sanitize_key, temp_sibling, unix_now};
use crate::error::Result;
use crate::model::MediaInfo;

const SCHEMA_VERSION: u32 = 1;
const DEFAULT_MAX_ENTRIES: usize = 256;
const SESSION_COOKIE_NAMES: [&str; 3] = ["ttwid", "tt_chain_token", "tt_csrf_token"];

/// One resolved metadata entry loaded from disk.
#[derive(Debug, Clone)]
pub struct CachedMetadata {
    /// Resolved media metadata.
    pub info: MediaInfo,
    /// Server-set media session cookies, filtered to a small allow-list.
    pub cookie: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct MetadataEnvelope {
    version: u32,
    key: String,
    expires_unix: u64,
    info: MediaInfo,
    session_cookie: Option<String>,
}

/// Disk-backed metadata/signed-URL cache.
#[derive(Debug, Clone)]
pub struct MetadataCache {
    root: PathBuf,
    ttl: Duration,
    max_entries: usize,
}

impl MetadataCache {
    /// Create a cache rooted at `root`.
    pub fn new(root: impl Into<PathBuf>, ttl: Duration) -> Self {
        Self {
            root: root.into(),
            ttl,
            max_entries: DEFAULT_MAX_ENTRIES,
        }
    }

    /// Create a cache with an explicit entry bound.
    pub fn with_capacity(root: impl Into<PathBuf>, ttl: Duration, max_entries: usize) -> Self {
        Self {
            root: root.into(),
            ttl,
            max_entries: max_entries.max(1),
        }
    }

    /// Cache root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Read a fresh entry and remove it when expired, malformed or from an
    /// older schema.
    pub async fn get(&self, key: &str) -> Option<CachedMetadata> {
        let path = self.entry_path(key);
        let bytes = fs::read(&path).await.ok()?;
        let envelope: MetadataEnvelope = match serde_json::from_slice(&bytes) {
            Ok(envelope) => envelope,
            Err(error) => {
                warn!(?error, path = %path.display(), "invalid metadata-cache entry");
                let _ = fs::remove_file(&path).await;
                return None;
            }
        };
        if envelope.version != SCHEMA_VERSION {
            let _ = fs::remove_file(&path).await;
            return None;
        }
        if envelope.expires_unix <= unix_now() {
            let _ = fs::remove_file(&path).await;
            return None;
        }
        if !envelope.info.has_media() {
            let _ = fs::remove_file(&path).await;
            return None;
        }

        Some(CachedMetadata {
            info: envelope.info,
            cookie: envelope.session_cookie,
        })
    }

    /// Write one metadata entry atomically.
    pub async fn put(
        &self,
        key: &str,
        info: &MediaInfo,
        session_cookie: Option<&str>,
    ) -> Result<()> {
        if !info.has_media() {
            return Ok(());
        }
        fs::create_dir_all(&self.root).await?;
        restrict_directory_permissions(&self.root).await;
        let ttl = media_ttl(info, self.ttl);
        let envelope = MetadataEnvelope {
            version: SCHEMA_VERSION,
            key: key.to_owned(),
            expires_unix: unix_now().saturating_add(ttl.as_secs()),
            info: info.clone(),
            session_cookie: filter_session_cookie(session_cookie),
        };
        let bytes = serde_json::to_vec(&envelope)?;
        let path = self.entry_path(key);
        let temp = temp_sibling(&path);
        fs::write(&temp, bytes).await?;
        restrict_permissions(&temp).await?;
        fs::rename(&temp, &path).await?;
        self.enforce_capacity().await;
        Ok(())
    }

    /// Remove one metadata entry.
    pub async fn invalidate(&self, key: &str) -> Result<bool> {
        let path = self.entry_path(key);
        match fs::remove_file(&path).await {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    /// Remove expired, malformed and older-schema entries. Returns removed files.
    pub async fn prune(&self) -> Result<usize> {
        let mut removed = 0usize;
        let Ok(mut entries) = fs::read_dir(&self.root).await else {
            return Ok(0);
        };
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let Ok(bytes) = fs::read(&path).await else {
                continue;
            };
            let valid = serde_json::from_slice::<MetadataEnvelope>(&bytes).is_ok_and(|envelope| {
                envelope.version == SCHEMA_VERSION
                    && envelope.expires_unix > unix_now()
                    && envelope.info.has_media()
            });
            if !valid && fs::remove_file(&path).await.is_ok() {
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Remove the whole metadata cache.
    pub async fn clear(&self) -> Result<()> {
        if fs::metadata(&self.root).await.is_ok() {
            fs::remove_dir_all(&self.root).await?;
        }
        Ok(())
    }

    async fn enforce_capacity(&self) {
        let mut files = Vec::new();
        let Ok(mut entries) = fs::read_dir(&self.root).await else {
            return;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            if entry.path().extension().is_some_and(|ext| ext == "json") {
                let modified = entry
                    .metadata()
                    .await
                    .ok()
                    .and_then(|metadata| metadata.modified().ok());
                files.push((modified, entry.path()));
            }
        }
        if files.len() <= self.max_entries {
            return;
        }
        files.sort_by_key(|(modified, _)| *modified);
        let remove = files.len() - self.max_entries;
        for (_, path) in files.into_iter().take(remove) {
            let _ = fs::remove_file(path).await;
        }
    }

    fn entry_path(&self, key: &str) -> PathBuf {
        self.root.join(format!("{}.json", sanitize_key(key)))
    }
}

/// Default per-user metadata root.
///
/// Order: `YSTT_CACHE_DIR`, `XDG_CACHE_HOME`, `HOME/.cache`, temp directory.
pub fn default_metadata_root() -> PathBuf {
    if let Some(root) = std::env::var_os("YSTT_CACHE_DIR") {
        return PathBuf::from(root).join("metadata");
    }
    if let Some(cache) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(cache).join("ystt").join("metadata");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".cache")
            .join("ystt")
            .join("metadata");
    }
    std::env::temp_dir().join("ystt").join("metadata")
}

/// Keep only server session cookies needed by TikTok's media CDN.
pub fn filter_session_cookie(header: Option<&str>) -> Option<String> {
    let header = header?;
    let mut kept = Vec::new();
    for pair in header.split(';') {
        let pair = pair.trim();
        let Some((name, value)) = pair.split_once('=') else {
            continue;
        };
        if SESSION_COOKIE_NAMES.contains(&name) && !value.is_empty() {
            kept.push(format!("{name}={value}"));
        }
    }
    (!kept.is_empty()).then(|| kept.join("; "))
}

async fn restrict_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

async fn restrict_directory_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).await;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Author, MediaInfo};

    fn info(video_url: &str) -> MediaInfo {
        MediaInfo {
            post_id: "1".into(),
            source_url: "https://www.tiktok.com/@u/video/1".into(),
            page_url: "https://www.tiktok.com/@u/video/1".into(),
            author: Author {
                id: "u".into(),
                nickname: None,
            },
            description: None,
            video_url: Some(video_url.into()),
            photo_urls: Vec::new(),
            photo_dimensions: Vec::new(),
            audio_url: None,
            is_slideshow: false,
            is_age_restricted: false,
            music: None,
            bitrates: Vec::new(),
            video_width: None,
            video_height: None,
            duration_sec: None,
        }
    }

    #[tokio::test]
    async fn roundtrip_and_invalidate() {
        let dir = tempfile::tempdir().unwrap();
        let cache = MetadataCache::new(dir.path(), Duration::from_secs(60));
        cache
            .put(
                "1:balanced",
                &info("https://cdn/v.mp4"),
                Some("sessionid=secret; ttwid=abc; tt_chain_token=tok; foo=bar"),
            )
            .await
            .unwrap();
        let entry = cache.get("1:balanced").await.unwrap();
        assert_eq!(entry.info.post_id, "1");
        assert_eq!(
            entry.cookie.as_deref(),
            Some("ttwid=abc; tt_chain_token=tok")
        );
        assert!(cache.invalidate("1:balanced").await.unwrap());
        assert!(cache.get("1:balanced").await.is_none());
    }

    #[tokio::test]
    async fn expired_and_malformed_entries_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let cache = MetadataCache::new(dir.path(), Duration::from_secs(1));
        let path = cache.entry_path("x");
        fs::create_dir_all(dir.path()).await.unwrap();
        fs::write(&path, b"not json").await.unwrap();
        assert!(cache.get("x").await.is_none());
        assert!(!path.exists());

        let expired = MetadataEnvelope {
            version: SCHEMA_VERSION,
            key: "x".into(),
            expires_unix: 1,
            info: info("https://cdn/v.mp4"),
            session_cookie: None,
        };
        fs::write(&path, serde_json::to_vec(&expired).unwrap())
            .await
            .unwrap();
        assert!(cache.get("x").await.is_none());
        assert!(!path.exists());

        let old_schema = MetadataEnvelope {
            version: SCHEMA_VERSION + 1,
            key: "x".into(),
            expires_unix: unix_now() + 60,
            info: info("https://cdn/v.mp4"),
            session_cookie: None,
        };
        fs::write(&path, serde_json::to_vec(&old_schema).unwrap())
            .await
            .unwrap();
        assert!(cache.get("x").await.is_none());
        assert!(!path.exists());
    }

    #[test]
    fn cookie_filter_keeps_only_media_session_cookies() {
        let filtered = filter_session_cookie(Some(
            "sessionid=account; ttwid=a; tt_chain_token=b; tt_csrf_token=c; x=y",
        ));
        assert_eq!(
            filtered.as_deref(),
            Some("ttwid=a; tt_chain_token=b; tt_csrf_token=c")
        );
        assert!(filter_session_cookie(None).is_none());
    }

    #[test]
    fn keys_are_path_safe() {
        assert_eq!(sanitize_key("1:balanced"), "1_balanced");
        assert_eq!(sanitize_key("../../x"), "______x");
    }
}
