//! Environment configuration for the HTTP API.

use std::fmt;
use std::net::SocketAddr;
use std::str::FromStr;
use std::time::Duration;

use thiserror::Error;

/// API configuration errors.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// A required environment variable is absent.
    #[error("missing environment variable {0}")]
    Missing(&'static str),
    /// A configured value cannot be parsed or is unsafe.
    #[error("invalid {name}: {value}")]
    Invalid {
        /// Variable name.
        name: &'static str,
        /// Rejected value.
        value: String,
    },
}

/// Immutable API server configuration.
#[derive(Clone)]
pub struct ApiConfig {
    /// Bearer key required for every endpoint.
    pub api_key: String,
    /// Local bind address.
    pub bind: SocketAddr,
    /// Public base URL, including the `/api` prefix when proxied.
    pub public_base: String,
    /// Per-media byte cap.
    pub max_bytes: u64,
    /// In-memory media TTL.
    pub media_ttl: Duration,
    /// Total in-memory media-store cap.
    pub store_bytes: usize,
    /// Concurrent downloader workers.
    pub workers: usize,
    /// Maximum concurrent items inside a batch.
    pub batch_concurrency: usize,
    /// Maximum URLs in one batch.
    pub max_batch: usize,
    /// Requests per minute per client.
    pub rate_per_min: u32,
    /// Token-bucket burst capacity.
    pub rate_burst: u32,
}

impl fmt::Debug for ApiConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ApiConfig")
            .field("bind", &self.bind)
            .field("public_base", &self.public_base)
            .field("max_bytes", &self.max_bytes)
            .field("media_ttl", &self.media_ttl)
            .field("store_bytes", &self.store_bytes)
            .field("workers", &self.workers)
            .field("batch_concurrency", &self.batch_concurrency)
            .field("max_batch", &self.max_batch)
            .field("rate_per_min", &self.rate_per_min)
            .field("rate_burst", &self.rate_burst)
            .finish_non_exhaustive()
    }
}

impl ApiConfig {
    /// Read configuration from environment variables.
    pub fn from_env() -> Result<Self, ConfigError> {
        let mut public_base = required("YSTT_API_PUBLIC_BASE")
            .unwrap_or_else(|_| "https://ystt.d9v.uk/api".to_owned());
        while public_base.ends_with('/') {
            public_base.pop();
        }
        if !public_base.starts_with("https://")
            && env("YSTT_API_ALLOW_HTTP").as_deref() != Some("1")
        {
            return Err(ConfigError::Invalid {
                name: "YSTT_API_PUBLIC_BASE",
                value: public_base,
            });
        }
        Ok(Self {
            api_key: required("YSTT_API_KEY")?,
            bind: parse_or("YSTT_API_BIND", "127.0.0.1:8091")?,
            public_base,
            max_bytes: parse_or::<u64>("YSTT_API_MAX_MB", "128")? * 1024 * 1024,
            media_ttl: Duration::from_secs(parse_or("YSTT_API_MEDIA_TTL", "900")?),
            store_bytes: parse_or::<usize>("YSTT_API_STORE_MB", "768")? * 1024 * 1024,
            workers: parse_or::<usize>("YSTT_API_WORKERS", "3")?.max(1),
            batch_concurrency: parse_or::<usize>("YSTT_API_BATCH_CONCURRENCY", "2")?.max(1),
            max_batch: parse_or::<usize>("YSTT_API_MAX_BATCH", "20")?.max(1),
            rate_per_min: parse_or::<u32>("YSTT_API_RATE_PER_MIN", "120")?.max(1),
            rate_burst: parse_or::<u32>("YSTT_API_RATE_BURST", "30")?.max(1),
        })
    }

    /// Build an authenticated media URL for a token.
    pub fn media_url(&self, token: &str) -> String {
        format!("{}/v1/media/{token}", self.public_base)
    }
}

fn env(name: &'static str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn required(name: &'static str) -> Result<String, ConfigError> {
    env(name).ok_or(ConfigError::Missing(name))
}

fn parse_or<T>(name: &'static str, default: &str) -> Result<T, ConfigError>
where
    T: FromStr,
{
    let raw = env(name).unwrap_or_else(|| default.to_owned());
    raw.parse()
        .map_err(|_| ConfigError::Invalid { name, value: raw })
}
