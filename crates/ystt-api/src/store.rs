//! Bounded in-memory media store. Nothing is written to disk.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use serde::Serialize;
use utoipa::ToSchema;
use ystt::MediaKind;

/// Media bytes held in memory until their token expires.
#[derive(Debug, Clone)]
pub struct StoredMedia {
    /// Payload.
    pub bytes: Bytes,
    /// MIME type.
    pub mime: String,
    /// Extension without a dot.
    pub ext: String,
    /// Media kind.
    pub kind: MediaKind,
    /// SHA-256 digest.
    pub sha256: String,
    /// Original CDN URL.
    pub source_url: String,
    /// Insertion time.
    pub created: Instant,
    /// Expiry time.
    pub expires: Instant,
}

/// Current media-store statistics.
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
pub struct MediaStats {
    /// Number of stored items.
    pub entries: usize,
    /// Total stored bytes.
    pub bytes: usize,
    /// Configured byte cap.
    pub max_bytes: usize,
}

struct Entry {
    media: Arc<StoredMedia>,
}

/// Bounded, expiring, process-local media store.
#[derive(Clone)]
pub struct MediaStore {
    inner: Arc<RwLock<HashMap<String, Entry>>>,
    ttl: Duration,
    max_bytes: usize,
    max_entries: usize,
}

impl MediaStore {
    /// Create a store with a TTL and byte cap.
    pub fn new(ttl: Duration, max_bytes: usize) -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
            ttl,
            max_bytes: max_bytes.max(1),
            max_entries: 4096,
        }
    }

    /// Insert media and return its opaque token.
    pub fn insert(&self, media: StoredMedia) -> String {
        let token = format!("{:032x}", rand::random::<u128>());
        let now = Instant::now();
        let media = Arc::new(StoredMedia {
            created: now,
            expires: now + self.ttl,
            ..media
        });
        let mut entries = self.inner.write().expect("media store lock");
        entries.retain(|_, entry| entry.media.expires > now);
        while !entries.is_empty()
            && (entries.len() >= self.max_entries
                || entries
                    .values()
                    .map(|entry| entry.media.bytes.len())
                    .sum::<usize>()
                    + media.bytes.len()
                    > self.max_bytes)
        {
            let oldest = entries
                .iter()
                .min_by_key(|(_, entry)| entry.media.created)
                .map(|(token, _)| token.clone());
            let Some(oldest) = oldest else {
                break;
            };
            entries.remove(&oldest);
        }
        entries.insert(token.clone(), Entry { media });
        token
    }

    /// Read a live media item.
    pub fn get(&self, token: &str) -> Option<Arc<StoredMedia>> {
        let now = Instant::now();
        let mut entries = self.inner.write().expect("media store lock");
        match entries.get(token) {
            Some(entry) if entry.media.expires > now => Some(entry.media.clone()),
            Some(_) => {
                entries.remove(token);
                None
            }
            None => None,
        }
    }

    /// Current storage statistics.
    pub fn stats(&self) -> MediaStats {
        let entries = self.inner.read().expect("media store lock");
        MediaStats {
            entries: entries.len(),
            bytes: entries.values().map(|entry| entry.media.bytes.len()).sum(),
            max_bytes: self.max_bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn media(size: usize) -> StoredMedia {
        StoredMedia {
            bytes: Bytes::from(vec![0u8; size]),
            mime: "video/mp4".into(),
            ext: "mp4".into(),
            kind: MediaKind::Video,
            sha256: "x".into(),
            source_url: "https://cdn/v".into(),
            created: Instant::now(),
            expires: Instant::now(),
        }
    }

    #[test]
    fn evicts_when_full() {
        let store = MediaStore::new(Duration::from_secs(60), 10);
        let first = store.insert(media(8));
        let second = store.insert(media(8));
        assert!(store.get(&first).is_none());
        assert!(store.get(&second).is_some());
    }

    #[test]
    fn concurrent_inserts_and_gets_are_safe() {
        let store = MediaStore::new(Duration::from_secs(60), 1024 * 1024);
        let mut handles = Vec::new();
        for _ in 0..8 {
            let store = store.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..100 {
                    let token = store.insert(media(64));
                    assert!(store.get(&token).is_some());
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        assert!(store.stats().entries > 0);
    }
}
