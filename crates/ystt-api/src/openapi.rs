//! Generated OpenAPI 3 document.

use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, OpenApi};

struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        if let Some(components) = openapi.components.as_mut() {
            components.add_security_scheme(
                "bearer_auth",
                SecurityScheme::Http(
                    HttpBuilder::new()
                        .scheme(HttpAuthScheme::Bearer)
                        .bearer_format("opaque")
                        .build(),
                ),
            );
        }
    }
}

/// Complete generated OpenAPI document.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "ystt API",
        version = env!("CARGO_PKG_VERSION"),
        description = "Authenticated, in-memory TikTok download API. All /v1 endpoints require a Bearer key."
    ),
    servers((url = "https://ystt.d9v.uk/api")),
    paths(
        crate::routes::health,
        crate::routes::resolve,
        crate::routes::download,
        crate::routes::batch,
        crate::routes::stats,
        crate::routes::openapi,
        crate::routes::media,
    ),
    components(schemas(
        crate::dto::DownloadOptionsInput,
        crate::dto::ResolveRequest,
        crate::dto::DownloadRequest,
        crate::dto::BatchRequest,
        crate::dto::MediaDescriptor,
        crate::dto::DownloadResponse,
        crate::dto::BatchItem,
        crate::dto::BatchResponse,
        crate::dto::ResolveResponse,
        crate::dto::HealthResponse,
        crate::dto::StatsResponse,
        crate::error::ErrorBody,
        crate::error::ErrorResponse,
        crate::store::MediaStats,
        ystt::Author,
        ystt::MediaInfo,
        ystt::MediaKind,
        ystt::PartialFailure,
        ystt::QualityPolicy,
        ystt::Timings,
    )),
    modifiers(&SecurityAddon),
    security(("bearer_auth" = []))
)]
/// Versioned schema exposed at `/v1/openapi.json`.
pub struct ApiDoc;
