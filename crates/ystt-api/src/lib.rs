//! Typed, authenticated HTTP API for `ystt`.
//!
//! The crate is both a runnable server and a library: use [`router`] with an
//! [`AppState`] to embed the API in another Axum service.

pub mod config;
pub mod downloader;
pub mod dto;
pub mod error;
pub mod openapi;
pub mod ratelimit;
pub mod routes;
pub mod state;
pub mod store;

use std::time::Duration;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::http::StatusCode;
use axum::middleware;
use axum::routing::{get, post};
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

pub use config::ApiConfig;
pub use error::{ApiError, ErrorBody, ErrorResponse};
pub use state::AppState;

/// Build the authenticated API router.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/health", get(routes::health))
        .route("/v1/resolve", post(routes::resolve))
        .route("/v1/download", post(routes::download))
        .route("/v1/batch", post(routes::batch))
        .route("/v1/stats", get(routes::stats))
        .route("/v1/openapi.json", get(routes::openapi))
        .route(
            "/v1/media/{token}",
            get(routes::media).head(routes::media_head),
        )
        .layer(middleware::from_fn_with_state(state.clone(), state::auth))
        .layer(DefaultBodyLimit::max(64 * 1024))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(300),
        ))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
