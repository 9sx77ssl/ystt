//! In-process download service and conversion to API descriptors.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{StreamExt, stream};
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;
use ystt::{Http2Tuning, InMemoryFile, InMemoryPost, MediaInfo, QualityPolicy, Ystt};

use crate::config::ApiConfig;
use crate::dto::{
    BatchItem, BatchResponse, DownloadOptionsInput, DownloadResponse, MediaDescriptor,
};
use crate::error::ApiError;
use crate::store::{MediaStore, StoredMedia};

/// Shared in-process download service.
#[derive(Clone)]
pub struct Downloader {
    ystt: Ystt,
    permits: Arc<Semaphore>,
    store: MediaStore,
    config: Arc<ApiConfig>,
}

impl Downloader {
    /// Build the downloader once and reuse its HTTP connection pool.
    pub fn new(config: Arc<ApiConfig>) -> Result<Self, ApiError> {
        let ystt = Ystt::builder()
            .quality(QualityPolicy::Balanced)
            .max_bytes(config.max_bytes)
            .file_cache(false)
            .metadata_cache(false)
            .parallel_ranges(true)
            .max_connections(6)
            .range_threshold(8 * 1024 * 1024)
            .photo_concurrency(8)
            .pool_max_idle_per_host(8)
            .per_host_concurrency(4)
            .timeout(Duration::from_secs(120))
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(120))
            .retries(2)
            .http2_tuning(Http2Tuning {
                connection_window: Some(64 * 1024 * 1024),
                keep_alive_interval: Some(Duration::from_secs(30)),
                keep_alive_timeout: Some(Duration::from_secs(5)),
                keep_alive_while_idle: Some(true),
            })
            .build()?;
        let store = MediaStore::new(config.media_ttl, config.store_bytes);
        Ok(Self {
            ystt,
            permits: Arc::new(Semaphore::new(config.workers.max(1))),
            store,
            config,
        })
    }

    /// Shared bounded media store.
    pub fn store(&self) -> &MediaStore {
        &self.store
    }

    /// Effective API configuration.
    pub fn config(&self) -> &ApiConfig {
        &self.config
    }

    /// Resolve one URL without downloading media bytes.
    pub async fn resolve(
        &self,
        url: &str,
        input: &DownloadOptionsInput,
    ) -> Result<MediaInfo, ApiError> {
        let options = input.to_options(&self.config)?;
        self.ystt
            .resolve_with(url, &options)
            .await
            .map_err(ApiError::from)
    }

    /// Download one post into memory and return authenticated descriptors.
    pub async fn download(
        &self,
        url: &str,
        input: &DownloadOptionsInput,
    ) -> Result<DownloadResponse, ApiError> {
        let _permit = self.permits.acquire().await.map_err(|_| {
            ApiError::new(
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "queue_closed",
                "download queue closed",
            )
        })?;
        let options = input.to_options(&self.config)?;
        let post = self
            .ystt
            .download_bytes_with(url, options)
            .await
            .map_err(ApiError::from)?;
        self.response(post)
    }

    /// Download multiple URLs with bounded concurrency, preserving order.
    pub async fn batch(
        &self,
        urls: Vec<String>,
        concurrency: Option<usize>,
        input: &DownloadOptionsInput,
    ) -> Result<BatchResponse, ApiError> {
        if urls.is_empty() {
            return Err(ApiError::new(
                axum::http::StatusCode::BAD_REQUEST,
                "invalid_request",
                "urls must not be empty",
            ));
        }
        if urls.len() > self.config.max_batch {
            return Err(ApiError::new(
                axum::http::StatusCode::BAD_REQUEST,
                "batch_too_large",
                format!("at most {} URLs are allowed", self.config.max_batch),
            ));
        }
        let started = Instant::now();
        let concurrency = concurrency
            .unwrap_or(self.config.batch_concurrency)
            .clamp(1, self.config.batch_concurrency);
        let total = urls.len();
        let mut results: Vec<Option<BatchItem>> = vec![None; total];
        let mut items = stream::iter(urls.into_iter().enumerate().map(|(index, url)| {
            let downloader = self.clone();
            let input = input.clone();
            async move {
                let result = downloader.download(&url, &input).await;
                let item = match result {
                    Ok(result) => BatchItem {
                        index,
                        url,
                        result: Some(result),
                        error: None,
                    },
                    Err(error) => BatchItem {
                        index,
                        url,
                        result: None,
                        error: Some(error.body.clone()),
                    },
                };
                (index, item)
            }
        }))
        .buffer_unordered(concurrency);
        while let Some((index, item)) = items.next().await {
            results[index] = Some(item);
        }
        let items: Vec<BatchItem> = results.into_iter().flatten().collect();
        let successes = items.iter().filter(|item| item.result.is_some()).count();
        let failures = items.len().saturating_sub(successes);
        Ok(BatchResponse {
            items,
            successes,
            failures,
            total_ms: started.elapsed().as_millis() as u64,
        })
    }

    fn response(&self, post: InMemoryPost) -> Result<DownloadResponse, ApiError> {
        let video = post.video.map(|file| self.describe(file));
        let photos: Vec<MediaDescriptor> = post
            .photos
            .into_iter()
            .map(|file| self.describe(file))
            .collect();
        let audio = post.audio.map(|file| self.describe(file));
        if video.is_none() && photos.is_empty() && audio.is_none() {
            return Err(ApiError::from(ystt::YsttError::NoMedia));
        }
        Ok(DownloadResponse {
            post_id: post.post_id,
            source_url: post.source_url,
            page_url: post.page_url,
            author: post.author,
            description: post.description,
            cover_url: post.cover_url,
            video,
            photos,
            audio,
            failures: post.failures,
            timings: post.timings,
            cache_hit: false,
        })
    }

    fn describe(&self, file: InMemoryFile) -> MediaDescriptor {
        let sha256 = sha256_hex(&file.bytes);
        let token = self.store.insert(StoredMedia {
            bytes: file.bytes,
            mime: file.mime.clone(),
            ext: file.ext.clone(),
            kind: file.kind,
            sha256: sha256.clone(),
            source_url: file.source_url.clone(),
            created: Instant::now(),
            expires: Instant::now(),
        });
        MediaDescriptor {
            url: self.config.media_url(&token),
            token,
            kind: file.kind,
            mime: file.mime,
            ext: file.ext,
            size: file.size,
            sha256,
            source_url: file.source_url,
            width: file.width,
            height: file.height,
            duration_sec: file.duration_sec,
        }
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
