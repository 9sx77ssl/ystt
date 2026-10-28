//! Typed HTTP errors and the stable API error payload.

use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use ystt::{ErrorContext, YsttError};

/// Stable machine-readable error payload.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ErrorBody {
    /// Stable machine-readable error code.
    pub code: String,
    /// Human-readable message.
    pub message: String,
    /// Optional non-secret diagnostic context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<ErrorContext>,
}

/// Envelope returned for every non-2xx response.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ErrorResponse {
    /// Error details.
    pub error: ErrorBody,
}

/// Internal API error with its HTTP status and public payload.
#[derive(Debug, Clone)]
pub struct ApiError {
    /// HTTP status.
    pub status: StatusCode,
    /// Public error body.
    pub body: ErrorBody,
}

impl ApiError {
    /// Construct an API error.
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            body: ErrorBody {
                code: code.to_owned(),
                message: message.into(),
                context: None,
            },
        }
    }

    /// Missing or invalid Bearer key.
    pub fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "missing or invalid Bearer API key",
        )
    }

    /// Per-client rate limit exceeded.
    pub fn rate_limited() -> Self {
        Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "request rate limit exceeded",
        )
    }

    /// Resource not found or expired.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message)
    }
}

impl From<YsttError> for ApiError {
    fn from(error: YsttError) -> Self {
        let status = match &error {
            YsttError::InvalidUrl(_) | YsttError::UnsupportedUrl(_) => StatusCode::BAD_REQUEST,
            YsttError::AgeRestricted => StatusCode::FORBIDDEN,
            YsttError::Unavailable(_) | YsttError::NoMedia | YsttError::NoVideo => {
                StatusCode::NOT_FOUND
            }
            YsttError::FileTooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
            YsttError::Cancelled | YsttError::Timeout => StatusCode::GATEWAY_TIMEOUT,
            YsttError::Network(_) | YsttError::Waf(_) | YsttError::Parse(_) => {
                StatusCode::BAD_GATEWAY
            }
            YsttError::Partial(_) => StatusCode::UNPROCESSABLE_ENTITY,
            YsttError::FileExists(_) => StatusCode::CONFLICT,
            YsttError::Config(_)
            | YsttError::Io(_)
            | YsttError::Range(_)
            | YsttError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let info = error.info();
        Self {
            status,
            body: ErrorBody {
                code: info.code.to_owned(),
                message: info.message,
                context: info.context,
            },
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.body.code, self.body.message)
    }
}

impl std::error::Error for ApiError {}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (self.status, Json(ErrorResponse { error: self.body })).into_response();
        if self.status == StatusCode::UNAUTHORIZED {
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        response
    }
}
