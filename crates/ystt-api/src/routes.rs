//! HTTP route handlers and media streaming.

use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use utoipa::OpenApi;

use crate::dto::{
    BatchRequest, BatchResponse, DownloadRequest, DownloadResponse, HealthResponse, ResolveRequest,
    ResolveResponse, StatsResponse,
};
use crate::error::{ApiError, ErrorBody, ErrorResponse};
use crate::openapi::ApiDoc;
use crate::state::AppState;
use crate::store::StoredMedia;

#[utoipa::path(
    get,
    path = "/v1/health",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Service is healthy", body = HealthResponse),
        (status = 401, description = "Missing or invalid Bearer key", body = ErrorResponse)
    )
)]
/// Health/authentication probe.
pub async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".into(),
        version: env!("CARGO_PKG_VERSION").into(),
    })
}

#[utoipa::path(
    post,
    path = "/v1/resolve",
    request_body = ResolveRequest,
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Resolved TikTok or SoundCloud metadata", body = ResolveResponse),
        (status = 400, description = "Invalid request or unsupported URL", body = ErrorResponse),
        (status = 401, description = "Missing or invalid Bearer key", body = ErrorResponse),
        (status = 404, description = "Media is unavailable or private", body = ErrorResponse),
        (status = 429, description = "Rate limit exceeded", body = ErrorResponse),
        (status = 502, description = "Upstream provider or network failure", body = ErrorResponse)
    )
)]
/// Resolve metadata only.
pub async fn resolve(
    State(state): State<AppState>,
    Json(request): Json<ResolveRequest>,
) -> Result<Json<ResolveResponse>, ApiError> {
    let response = state
        .downloader
        .resolve(&request.url, &request.options)
        .await?;
    Ok(Json(response))
}

#[utoipa::path(
    post,
    path = "/v1/download",
    request_body = DownloadRequest,
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Downloaded media descriptors", body = DownloadResponse),
        (status = 400, description = "Invalid request, unsupported URL or oversized playlist", body = ErrorResponse),
        (status = 401, description = "Missing or invalid Bearer key", body = ErrorResponse),
        (status = 404, description = "Media is unavailable or private", body = ErrorResponse),
        (status = 413, description = "Media or request exceeds configured limit", body = ErrorResponse),
        (status = 422, description = "One or more playlist/gallery items failed", body = ErrorResponse),
        (status = 429, description = "Rate limit exceeded", body = ErrorResponse),
        (status = 502, description = "Upstream provider or network failure", body = ErrorResponse)
    )
)]
/// Download one post.
pub async fn download(
    State(state): State<AppState>,
    Json(request): Json<DownloadRequest>,
) -> Result<Json<DownloadResponse>, ApiError> {
    Ok(Json(
        state
            .downloader
            .download(&request.url, &request.options)
            .await?,
    ))
}

#[utoipa::path(
    post,
    path = "/v1/batch",
    request_body = BatchRequest,
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Ordered batch results", body = BatchResponse),
        (status = 400, description = "Invalid request or batch too large", body = ErrorResponse),
        (status = 401, description = "Missing or invalid Bearer key", body = ErrorResponse),
        (status = 429, description = "Rate limit exceeded", body = ErrorResponse)
    )
)]
/// Download a bounded batch.
pub async fn batch(
    State(state): State<AppState>,
    Json(request): Json<BatchRequest>,
) -> Result<Json<BatchResponse>, ApiError> {
    Ok(Json(
        state
            .downloader
            .batch(request.urls, request.concurrency, &request.options)
            .await?,
    ))
}

#[utoipa::path(
    get,
    path = "/v1/stats",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Media-store and queue statistics", body = StatsResponse),
        (status = 401, description = "Missing or invalid Bearer key", body = ErrorResponse)
    )
)]
/// Media-store and queue statistics.
pub async fn stats(State(state): State<AppState>) -> Json<StatsResponse> {
    Json(StatsResponse {
        media: state.store.stats(),
        workers: state.config.workers,
        batch_concurrency: state.config.batch_concurrency,
        media_ttl_secs: state.config.media_ttl.as_secs(),
        result_cache_entries: state.downloader.result_cache_entries(),
    })
}

#[utoipa::path(
    get,
    path = "/v1/openapi.json",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "OpenAPI 3.1 schema"),
        (status = 401, description = "Missing or invalid Bearer key", body = ErrorResponse)
    )
)]
/// Generated OpenAPI schema.
pub async fn openapi() -> Result<Response, ApiError> {
    let document = ApiDoc::openapi();
    let pretty = serde_json::to_string_pretty(&document).map_err(|error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!("failed to serialize OpenAPI document: {error}"),
        )
    })?;
    let mut response = Response::new(Body::from(format!("{pretty}\n")));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    Ok(response)
}

#[utoipa::path(
    get,
    path = "/v1/media/{token}",
    params(("token" = String, Path, description = "Media token returned by /v1/download")),
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Media bytes", content_type = "application/octet-stream"),
        (status = 206, description = "Partial media bytes", content_type = "application/octet-stream"),
        (status = 401, description = "Missing or invalid Bearer key", body = ErrorResponse),
        (status = 404, description = "Media token not found or expired", body = ErrorResponse),
        (status = 416, description = "Requested range is not satisfiable", body = ErrorResponse)
    )
)]
/// Download one media token.
pub async fn media(
    State(state): State<AppState>,
    Path(token): Path<String>,
    headers: HeaderMap,
) -> Response {
    match state.store.get(&token) {
        Some(item) => media_response(&item, &headers, true),
        None => ApiError::not_found("media token not found or expired").into_response(),
    }
}

#[utoipa::path(
    head,
    path = "/v1/media/{token}",
    params(("token" = String, Path, description = "Media token returned by /v1/download")),
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Media metadata / headers", content_type = "application/octet-stream"),
        (status = 401, description = "Missing or invalid Bearer key", body = ErrorResponse),
        (status = 404, description = "Media token not found or expired", body = ErrorResponse),
        (status = 416, description = "Requested range is not satisfiable", body = ErrorResponse)
    )
)]
/// HEAD metadata for one media token.
pub async fn media_head(
    State(state): State<AppState>,
    Path(token): Path<String>,
    headers: HeaderMap,
) -> Response {
    match state.store.get(&token) {
        Some(item) => media_response(&item, &headers, false),
        None => ApiError::not_found("media token not found or expired").into_response(),
    }
}

fn media_response(item: &Arc<StoredMedia>, headers: &HeaderMap, include_body: bool) -> Response {
    let total = item.bytes.len();
    let range = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| parse_range(value, total));

    let (status, start, end) = match range {
        Some(Ok((start, end))) => (StatusCode::PARTIAL_CONTENT, start, end),
        Some(Err(())) => {
            let mut response = Response::new(Body::empty());
            *response.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
            response.headers_mut().insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes */{total}")).expect("range header"),
            );
            return response;
        }
        None => (StatusCode::OK, 0, total.saturating_sub(1)),
    };

    let length = end.saturating_sub(start).saturating_add(1);
    let body = if include_body {
        Body::from(item.bytes.slice(start..=end))
    } else {
        Body::empty()
    };
    let mut response = Response::new(body);
    *response.status_mut() = status;
    let output = response.headers_mut();
    output.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&item.mime)
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    output.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&length.to_string()).expect("content length"),
    );
    output.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    output.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=600"),
    );
    if status == StatusCode::PARTIAL_CONTENT {
        output.insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {start}-{end}/{total}")).expect("content range"),
        );
    }
    response
}

fn parse_range(value: &str, total: usize) -> Option<Result<(usize, usize), ()>> {
    let value = value.strip_prefix("bytes=")?;
    if total == 0 || value.contains(',') {
        return Some(Err(()));
    }
    let (start, end) = value.split_once('-')?;
    let (start, end) = if start.is_empty() {
        let suffix = end.parse::<usize>().ok()?;
        if suffix == 0 {
            return Some(Err(()));
        }
        (total.saturating_sub(suffix), total - 1)
    } else {
        let start = start.parse::<usize>().ok()?;
        let end = if end.is_empty() {
            total - 1
        } else {
            end.parse::<usize>().ok()?.min(total - 1)
        };
        (start, end)
    };
    if start >= total || start > end {
        Some(Err(()))
    } else {
        Some(Ok((start, end)))
    }
}

// Keep the error schema linked from handler docs even when a route only returns
// a success body in its primary response.
#[allow(dead_code)]
fn _error_schema(_: ErrorBody) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_parser_works() {
        assert_eq!(parse_range("bytes=0-9", 100), Some(Ok((0, 9))));
        assert_eq!(parse_range("bytes=-10", 100), Some(Ok((90, 99))));
        assert_eq!(parse_range("bytes=100-", 100), Some(Err(())));
    }
}
