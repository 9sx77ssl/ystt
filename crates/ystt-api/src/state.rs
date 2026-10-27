//! Shared API state and Bearer authentication middleware.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use subtle::ConstantTimeEq;

use crate::config::ApiConfig;
use crate::downloader::Downloader;
use crate::error::ApiError;
use crate::ratelimit::RateLimiter;
use crate::store::MediaStore;

/// Shared router state.
#[derive(Clone)]
pub struct AppState {
    /// Configuration.
    pub config: Arc<ApiConfig>,
    /// Downloader and HTTP pool.
    pub downloader: Downloader,
    /// Media store.
    pub store: MediaStore,
    /// Per-client rate limiter.
    pub limiter: RateLimiter,
}

impl AppState {
    /// Construct state and its shared clients.
    pub fn new(config: ApiConfig) -> Result<Self, ApiError> {
        let config = Arc::new(config);
        let downloader = Downloader::new(config.clone())?;
        let store = downloader.store().clone();
        let limiter = RateLimiter::new(config.rate_per_min, config.rate_burst);
        Ok(Self {
            config,
            downloader,
            store,
            limiter,
        })
    }
}

/// Bearer-key and per-client rate-limit middleware.
pub async fn auth(
    State(state): State<AppState>,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Response {
    let Some(value) = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
    else {
        return ApiError::unauthorized().into_response();
    };
    let Some((scheme, key)) = value.split_once(' ') else {
        return ApiError::unauthorized().into_response();
    };
    if !scheme.eq_ignore_ascii_case("Bearer")
        || !bool::from(state.config.api_key.as_bytes().ct_eq(key.trim().as_bytes()))
    {
        return ApiError::unauthorized().into_response();
    }

    let client = client_key(&headers);
    if !state.limiter.allow(&client) {
        return ApiError::rate_limited().into_response();
    }
    next.run(request).await
}

fn client_key(headers: &HeaderMap) -> String {
    if let Some(value) = headers
        .get("x-real-ip")
        .and_then(|value| value.to_str().ok())
    {
        return value.to_owned();
    }
    if let Some(value) = headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
    {
        return value.trim().to_owned();
    }
    "unknown".to_owned()
}
