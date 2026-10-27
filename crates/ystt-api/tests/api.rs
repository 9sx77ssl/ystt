//! HTTP boundary tests for auth, schema and media ranges.

use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use bytes::Bytes;
use tower::ServiceExt;
use ystt::MediaKind;
use ystt_api::config::ApiConfig;
use ystt_api::store::StoredMedia;
use ystt_api::{AppState, router};

fn state() -> AppState {
    AppState::new(ApiConfig {
        api_key: "test-key".into(),
        bind: "127.0.0.1:0".parse().unwrap(),
        public_base: "https://api.test".into(),
        max_bytes: 16 * 1024 * 1024,
        media_ttl: Duration::from_secs(60),
        store_bytes: 1024 * 1024,
        workers: 2,
        batch_concurrency: 1,
        max_batch: 4,
        max_playlist_tracks: 16,
        rate_per_min: 600,
        rate_burst: 100,
    })
    .unwrap()
}

fn request(uri: &str, key: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().uri(uri);
    if let Some(key) = key {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {key}"));
    }
    builder.body(Body::empty()).unwrap()
}

#[tokio::test]
async fn bearer_is_required() {
    let app = router(state());
    let response = app
        .clone()
        .oneshot(request("/v1/health", None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(response.headers().contains_key(header::WWW_AUTHENTICATE));

    let response = app
        .oneshot(request("/v1/health", Some("test-key")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn openapi_contains_bearer_security() {
    let response = router(state())
        .oneshot(request("/v1/openapi.json", Some("test-key")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["openapi"], "3.1.0");
    assert_eq!(
        value["components"]["securitySchemes"]["bearer_auth"]["scheme"],
        "bearer"
    );
    assert!(value["paths"]["/v1/download"].is_object());
    assert!(value["components"]["schemas"]["SoundCloudEntity"].is_object());
    assert!(value["components"]["schemas"]["PlaylistDescriptor"].is_object());
    assert!(value["components"]["schemas"]["Provider"].is_object());
    assert!(
        bytes.windows(3).any(|window| window == b"\n  "),
        "OpenAPI response must be pretty-printed"
    );
}

#[tokio::test]
async fn media_supports_ranges() {
    let state = state();
    let token = state.store.insert(StoredMedia {
        bytes: Bytes::from_static(b"abcdef"),
        mime: "video/mp4".into(),
        ext: "mp4".into(),
        kind: MediaKind::Video,
        sha256: "hash".into(),
        source_url: "https://cdn/v".into(),
        created: Instant::now(),
        expires: Instant::now() + Duration::from_secs(60),
    });
    let response = router(state)
        .oneshot(
            Request::builder()
                .uri(format!("/v1/media/{token}"))
                .header(header::AUTHORIZATION, "Bearer test-key")
                .header(header::RANGE, "bytes=1-3")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 1-3/6");
    let body = axum::body::to_bytes(response.into_body(), 8).await.unwrap();
    assert_eq!(&body[..], b"bcd");
}
