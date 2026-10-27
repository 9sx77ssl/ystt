//! Content-addressed file cache.
//!
//! The cache stores immutable objects under `objects/<blake3>.<ext>` and a small
//! JSON manifest per `post_id:quality:audio` key. Materialising a hit copies the
//! object into the output directory, so user edits cannot corrupt the cache.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::fs;
use tracing::warn;

use crate::cache_util::{FileLock, acquire_lock, sanitize_key, temp_sibling, unix_now};
use crate::error::{Result, YsttError};
use crate::media::DownloadedFile;
use crate::model::MediaInfo;

/// One cached media object and the filename it should materialise to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedFile {
    /// Object stored in the cache; its `path` is the content-addressed object.
    pub file: DownloadedFile,
    /// Destination filename for the caller's output directory.
    pub filename: String,
}

/// Manifest for one resolved download.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileCacheEntry {
    /// Cache key.
    pub key: String,
    /// Resolved metadata.
    pub info: MediaInfo,
    /// Video object.
    pub video: Option<CachedFile>,
    /// Gallery objects in order.
    pub photos: Vec<CachedFile>,
    /// Audio object.
    pub audio: Option<CachedFile>,
    /// Creation time, Unix seconds.
    pub created_unix: u64,
}

/// Content-addressed file cache rooted at a directory.
#[derive(Debug, Clone)]
pub struct FileCache {
    root: PathBuf,
    ttl: Duration,
}

impl FileCache {
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

    /// Acquire an exclusive cross-process lock for one cache key.
    pub(crate) async fn lock(&self, key: &str) -> Result<FileLock> {
        let path = self
            .root
            .join("locks")
            .join(format!("{}.lock", sanitize_key(key)));
        Ok(acquire_lock(&path).await?)
    }

    /// Read a fresh manifest and verify every referenced object exists.
    pub async fn get(&self, key: &str) -> Option<FileCacheEntry> {
        let path = self.manifest_path(key);
        let bytes = fs::read(&path).await.ok()?;
        let entry: FileCacheEntry = match serde_json::from_slice(&bytes) {
            Ok(entry) => entry,
            Err(error) => {
                warn!(?error, path = %path.display(), "invalid file-cache manifest");
                let _ = fs::remove_file(&path).await;
                return None;
            }
        };

        if is_expired(entry.created_unix, self.ttl) {
            let _ = fs::remove_file(&path).await;
            return None;
        }

        for file in entry
            .video
            .iter()
            .chain(entry.photos.iter())
            .chain(entry.audio.iter())
            .map(|cached| &cached.file)
        {
            let Ok(metadata) = fs::metadata(&file.path).await else {
                let _ = fs::remove_file(&path).await;
                return None;
            };
            if !metadata.is_file() || metadata.len() != file.size {
                let _ = fs::remove_file(&path).await;
                return None;
            }
        }

        Some(entry)
    }

    /// Store a manifest atomically.
    pub async fn put(&self, key: &str, mut entry: FileCacheEntry) -> Result<()> {
        entry.key = key.to_owned();
        entry.created_unix = unix_now();
        let manifest = self.manifest_path(key);
        let parent = manifest
            .parent()
            .ok_or_else(|| YsttError::Internal("cache manifest has no parent".into()))?;
        fs::create_dir_all(parent).await?;

        let bytes = serde_json::to_vec(&entry)?;
        let temp = temp_sibling(&manifest);
        fs::write(&temp, bytes).await?;
        fs::rename(&temp, &manifest).await?;
        Ok(())
    }

    /// Remove one manifest. Returns whether it existed.
    ///
    /// Objects become unreferenced and are removed by the next [`Self::prune`].
    pub async fn invalidate(&self, key: &str) -> Result<bool> {
        let path = self.manifest_path(key);
        match fs::remove_file(&path).await {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    /// Insert a downloaded file into the object store and return its cached form.
    pub async fn cached_file(&self, file: &DownloadedFile) -> Result<CachedFile> {
        let (object, _, size) = self.store_file(&file.path, &file.ext).await?;
        let mut cached = file.clone();
        cached.path = object;
        cached.size = size;
        let filename = file
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .ok_or_else(|| YsttError::Internal("downloaded file has no name".into()))?;
        Ok(CachedFile {
            file: cached,
            filename,
        })
    }

    /// Hash a file, move/link it into the object store and return the object path.
    pub async fn store_file(&self, path: &Path, ext: &str) -> Result<(PathBuf, String, u64)> {
        let metadata = fs::metadata(path).await?;
        if !metadata.is_file() || metadata.len() == 0 {
            return Err(YsttError::Internal("cannot cache an empty file".into()));
        }
        let size = metadata.len();
        let source = path.to_path_buf();
        let hash = tokio::task::spawn_blocking(move || -> std::io::Result<String> {
            let file = std::fs::File::open(source)?;
            let mut hasher = blake3::Hasher::new();
            hasher.update_reader(file)?;
            Ok(hasher.finalize().to_hex().to_string())
        })
        .await
        .map_err(|error| YsttError::Internal(error.to_string()))??;

        let objects = self.root.join("objects");
        fs::create_dir_all(&objects).await?;
        let object = objects.join(format!("{hash}.{ext}"));
        if fs::metadata(&object).await.is_ok() {
            return Ok((object, hash, size));
        }

        let temp = temp_sibling(&object);
        fs::copy(path, &temp).await?;
        if let Err(error) = fs::rename(&temp, &object).await {
            let _ = fs::remove_file(&temp).await;
            if fs::metadata(&object).await.is_err() {
                return Err(error.into());
            }
        }
        Ok((object, hash, size))
    }

    /// Materialise an object at `destination`, reusing an existing identical file.
    pub async fn materialize(&self, object: &Path, destination: &Path) -> Result<()> {
        if object == destination {
            return Ok(());
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).await?;
        }

        let object_size = fs::metadata(object).await?.len();
        if let Ok(metadata) = fs::metadata(destination).await
            && metadata.is_file()
            && metadata.len() == object_size
        {
            return Ok(());
        }
        let _ = fs::remove_file(destination).await;

        let temp = temp_sibling(destination);
        let _ = fs::remove_file(&temp).await;

        fs::copy(object, &temp).await?;
        fs::rename(&temp, destination).await?;
        Ok(())
    }

    /// Remove stale manifests and unreferenced objects. Returns removed objects.
    pub async fn prune(&self) -> Result<usize> {
        let manifests = self.root.join("manifests");
        let objects = self.root.join("objects");
        let mut referenced = HashSet::new();
        let mut removed = 0usize;

        if let Ok(mut entries) = fs::read_dir(&manifests).await {
            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();
                let Ok(bytes) = fs::read(&path).await else {
                    continue;
                };
                let Ok(manifest) = serde_json::from_slice::<FileCacheEntry>(&bytes) else {
                    let _ = fs::remove_file(&path).await;
                    continue;
                };
                if is_expired(manifest.created_unix, self.ttl) {
                    let _ = fs::remove_file(&path).await;
                    continue;
                }
                for file in manifest
                    .video
                    .iter()
                    .chain(manifest.photos.iter())
                    .chain(manifest.audio.iter())
                {
                    referenced.insert(file.file.path.clone());
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

    /// Delete all manifests and objects.
    pub async fn clear(&self) -> Result<()> {
        for directory in ["manifests", "objects", "locks"] {
            let path = self.root.join(directory);
            if fs::metadata(&path).await.is_ok() {
                fs::remove_dir_all(path).await?;
            }
        }
        Ok(())
    }

    fn manifest_path(&self, key: &str) -> PathBuf {
        self.root
            .join("manifests")
            .join(format!("{}.json", sanitize_key(key)))
    }
}

fn is_expired(created_unix: u64, ttl: Duration) -> bool {
    unix_now().saturating_sub(created_unix) > ttl.as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Author, MediaInfo, MediaKind};
    use std::path::PathBuf;

    fn sample_info() -> MediaInfo {
        MediaInfo {
            post_id: "1".into(),
            source_url: "https://www.tiktok.com/@u/video/1".into(),
            page_url: "https://www.tiktok.com/@u/video/1".into(),
            author: Author {
                id: "u".into(),
                nickname: None,
            },
            description: None,
            video_url: Some("https://cdn/v.mp4".into()),
            photo_urls: vec![],
            photo_dimensions: vec![],
            audio_url: None,
            is_slideshow: false,
            is_age_restricted: false,
            music: None,
            bitrates: vec![],
            video_width: None,
            video_height: None,
            duration_sec: None,
        }
    }

    #[tokio::test]
    async fn caches_materializes_and_prunes() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FileCache::new(dir.path(), Duration::from_secs(3600));
        let source = dir.path().join("tiktok_u_1.mp4");
        fs::write(&source, b"0123456789").await.unwrap();

        let file = DownloadedFile {
            path: source.clone(),
            mime: "video/mp4".into(),
            ext: "mp4".into(),
            size: 10,
            url: "https://cdn/v.mp4".into(),
            kind: MediaKind::Video,
        };
        let cached = cache.cached_file(&file).await.unwrap();
        assert!(cached.file.path.starts_with(cache.root().join("objects")));
        assert_eq!(cached.file.path.extension().unwrap(), "mp4");

        let entry = FileCacheEntry {
            key: "1:best:0".into(),
            info: sample_info(),
            video: Some(cached),
            photos: vec![],
            audio: None,
            created_unix: 0,
        };
        cache.put("1:best:0", entry).await.unwrap();
        let loaded = cache.get("1:best:0").await.unwrap();
        let object = loaded.video.unwrap().file.path;

        assert!(cache.invalidate("1:best:0").await.unwrap());
        assert!(cache.get("1:best:0").await.is_none());
        assert!(!cache.invalidate("1:best:0").await.unwrap());

        let destination = dir.path().join("out.mp4");
        cache.materialize(&object, &destination).await.unwrap();
        assert_eq!(fs::read(&destination).await.unwrap(), b"0123456789");

        cache.clear().await.unwrap();
        assert!(cache.get("1:best:0").await.is_none());
    }

    #[tokio::test]
    async fn expired_manifest_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FileCache::new(dir.path(), Duration::from_secs(0));
        let manifest = cache.manifest_path("x");
        fs::create_dir_all(manifest.parent().unwrap())
            .await
            .unwrap();
        let entry = FileCacheEntry {
            key: "x".into(),
            info: sample_info(),
            video: None,
            photos: vec![],
            audio: None,
            created_unix: unix_now().saturating_sub(10),
        };
        fs::write(&manifest, serde_json::to_vec(&entry).unwrap())
            .await
            .unwrap();
        assert!(cache.get("x").await.is_none());
        assert!(!manifest.exists());
    }

    #[test]
    fn keys_are_sanitized() {
        assert_eq!(sanitize_key("1:best:0"), "1_best_0");
        assert_eq!(
            PathBuf::from(sanitize_key("../../x")).components().count(),
            1
        );
    }
}
