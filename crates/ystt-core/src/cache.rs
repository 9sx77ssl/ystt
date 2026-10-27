//! In-memory signed-URL cache.
//!
//! TikTok CDN URLs are signed and expire. The cache stores the resolved
//! [`MediaInfo`] only until shortly before the signed URL expires, so a repeat
//! download can skip the HTML page without reusing a dead URL.

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::RwLock;

use crate::model::MediaInfo;

#[derive(Debug, Clone)]
struct Entry {
    info: MediaInfo,
    expires_at: Instant,
}

/// Small async cache keyed by `post_id:quality`.
#[derive(Debug)]
pub struct UrlCache {
    ttl: Duration,
    max_entries: usize,
    entries: RwLock<HashMap<String, Entry>>,
}

impl UrlCache {
    /// Create a cache with a default TTL and a 256-entry bound.
    pub fn new(ttl: Duration) -> Self {
        Self::with_capacity(ttl, 256)
    }

    /// Create a cache with an explicit entry bound.
    pub fn with_capacity(ttl: Duration, max_entries: usize) -> Self {
        Self {
            ttl,
            max_entries: max_entries.max(1),
            entries: RwLock::new(HashMap::new()),
        }
    }

    /// Read a fresh entry.
    pub async fn get(&self, key: &str) -> Option<MediaInfo> {
        let now = Instant::now();
        {
            let entries = self.entries.read().await;
            if let Some(entry) = entries.get(key)
                && entry.expires_at > now
            {
                return Some(entry.info.clone());
            }
        }

        let mut entries = self.entries.write().await;
        match entries.get(key) {
            Some(entry) if entry.expires_at > Instant::now() => Some(entry.info.clone()),
            Some(_) => {
                entries.remove(key);
                None
            }
            None => None,
        }
    }

    /// Store an entry, computing a TTL from the signed URLs when possible.
    pub async fn put(&self, key: String, info: MediaInfo) {
        let ttl = media_ttl(&info, self.ttl);
        let mut entries = self.entries.write().await;
        if entries.len() >= self.max_entries {
            let now = Instant::now();
            entries.retain(|_, entry| entry.expires_at > now);
        }
        if entries.len() >= self.max_entries
            && let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, entry)| entry.expires_at)
                .map(|(key, _)| key.clone())
        {
            entries.remove(&oldest);
        }
        entries.insert(
            key,
            Entry {
                info,
                expires_at: Instant::now() + ttl,
            },
        );
    }

    /// Remove one entry.
    pub async fn invalidate(&self, key: &str) {
        self.entries.write().await.remove(key);
    }

    /// Remove everything.
    pub async fn clear(&self) {
        self.entries.write().await.clear();
    }

    /// Number of entries, including expired ones not yet swept.
    pub async fn len(&self) -> usize {
        self.entries.read().await.len()
    }

    /// Whether the cache is empty.
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }
}

pub(crate) fn media_ttl(info: &MediaInfo, default: Duration) -> Duration {
    let mut ttl = default;
    for url in info
        .video_url
        .iter()
        .chain(info.photo_urls.iter())
        .chain(info.audio_url.iter())
    {
        ttl = ttl.min(signed_url_ttl(url, default));
    }
    ttl.max(Duration::from_secs(10))
}

/// Derive a safe TTL from common signed-URL expiry parameters.
///
/// TikTok CDN URLs use `expire` or `x-expires` as Unix seconds. Values that
/// look like milliseconds are handled too. The result is clamped to
/// `[10 seconds, 15 minutes]` and is 30 seconds shorter than the URL expiry to
/// avoid racing the signature.
pub fn signed_url_ttl(url: &str, default: Duration) -> Duration {
    let Ok(url) = url::Url::parse(url) else {
        return default;
    };

    let expiry = url.query_pairs().find_map(|(key, value)| {
        if !matches!(
            key.to_ascii_lowercase().as_str(),
            "expire" | "x-expires" | "x_expires"
        ) {
            return None;
        }
        value.parse::<u64>().ok()
    });

    let Some(expiry) = expiry else {
        return default;
    };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);

    let expiry = if expiry > 10_000_000_000 {
        expiry / 1000
    } else {
        expiry
    };

    let remaining = expiry.saturating_sub(now).saturating_sub(30);
    Duration::from_secs(remaining).clamp(Duration::from_secs(10), Duration::from_secs(15 * 60))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Author, MediaInfo};

    fn info(video_url: Option<String>) -> MediaInfo {
        MediaInfo {
            post_id: "1".into(),
            source_url: "https://www.tiktok.com/@u/video/1".into(),
            page_url: "https://www.tiktok.com/@u/video/1".into(),
            author: Author {
                id: "u".into(),
                nickname: None,
            },
            description: None,
            video_url,
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
            cover_url: None,
        }
    }

    #[tokio::test]
    async fn cache_roundtrip_and_invalidate() {
        let cache = UrlCache::new(Duration::from_secs(60));
        cache
            .put("1:best".into(), info(Some("https://cdn/v.mp4".into())))
            .await;
        assert!(cache.get("1:best").await.is_some());
        cache.invalidate("1:best").await;
        assert!(cache.get("1:best").await.is_none());
    }

    #[tokio::test]
    async fn cache_is_bounded() {
        let cache = UrlCache::with_capacity(Duration::from_secs(60), 2);
        for index in 0..4 {
            cache
                .put(
                    format!("{index}:best"),
                    info(Some("https://cdn/v.mp4".into())),
                )
                .await;
        }
        assert!(cache.len().await <= 2);
    }

    #[test]
    fn ttl_uses_expire_parameter() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let url = format!("https://cdn/v.mp4?expire={}", now + 600);
        let ttl = signed_url_ttl(&url, Duration::from_secs(60));
        assert!(ttl >= Duration::from_secs(540));
        assert!(ttl <= Duration::from_secs(600));
    }

    #[test]
    fn expired_url_gets_minimum_ttl() {
        let url = "https://cdn/v.mp4?expire=1";
        assert_eq!(
            signed_url_ttl(url, Duration::from_secs(60)),
            Duration::from_secs(10)
        );
    }

    #[test]
    fn missing_expiry_uses_default() {
        assert_eq!(
            signed_url_ttl("https://cdn/v.mp4", Duration::from_secs(42)),
            Duration::from_secs(42)
        );
    }

    #[test]
    fn milliseconds_are_normalized() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let url = format!("https://cdn/v.mp4?x-expires={}", (now + 300) * 1000);
        let ttl = signed_url_ttl(&url, Duration::from_secs(60));
        assert!(ttl <= Duration::from_secs(300));
    }
}
