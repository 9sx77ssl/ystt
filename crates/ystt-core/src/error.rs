//! Typed errors for the public `ystt` API.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use ystt_net::NetError;
use ystt_waf::WafError;

use crate::model::MediaKind;

/// Result alias used across `ystt-core`.
pub type Result<T> = std::result::Result<T, YsttError>;

/// One media item that failed during a partial gallery download.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct PartialFailure {
    /// Media kind.
    pub kind: MediaKind,
    /// Source URL.
    pub url: String,
    /// Human-readable failure.
    pub error: String,
}

/// Non-secret context attached to an error for debugging and JSON APIs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ErrorContext {
    /// Provider guessed from the failed URL.
    pub provider: Option<String>,
    /// Host that failed, when known.
    pub host: Option<String>,
    /// Media kind, when known.
    pub kind: Option<MediaKind>,
    /// HTTP status, when known.
    pub status: Option<u16>,
    /// Attempt number, when known.
    pub attempt: Option<u32>,
}

/// Stable, serializable error payload for HTTP APIs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorInfo {
    /// Stable machine-readable error code.
    pub code: &'static str,
    /// Human-readable message.
    pub message: String,
    /// Optional structured context. Never contains credentials or cookies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<ErrorContext>,
}

/// Errors returned by the downloader.
#[derive(Debug, Error)]
pub enum YsttError {
    /// The input is not a URL at all.
    #[error("invalid URL: {0}")]
    InvalidUrl(String),

    /// Configuration is invalid.
    #[error("invalid configuration: {0}")]
    Config(String),

    /// The URL is valid but not a TikTok URL.
    #[error("not a TikTok URL: {0}")]
    UnsupportedUrl(String),

    /// The provider says the post/track is gone, private or unavailable.
    #[error("media is unavailable: {0}")]
    Unavailable(String),

    /// TikTok marks the post as age-restricted and exposes no media URL.
    #[error("TikTok post is age-restricted")]
    AgeRestricted,

    /// The HTTP layer failed.
    #[error(transparent)]
    Network(#[from] NetError),

    /// The WAF solver failed.
    #[error(transparent)]
    Waf(#[from] WafError),

    /// The page or item JSON could not be parsed.
    #[error("failed to parse TikTok page: {0}")]
    Parse(String),

    /// The post contains no video, photos or audio.
    #[error("no media found in post")]
    NoMedia,

    /// The post has media, but no downloadable video.
    #[error("post has no downloadable video")]
    NoVideo,

    /// A media file exceeded the configured cap.
    #[error("file is too large: {size} bytes exceeds {limit}")]
    FileTooLarge {
        /// Actual size.
        size: u64,
        /// Configured limit.
        limit: u64,
    },

    /// The requested output path already exists under the fail policy.
    #[error("output file already exists: {0}")]
    FileExists(PathBuf),

    /// Filesystem failure.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// The caller cancelled the operation.
    #[error("cancelled")]
    Cancelled,

    /// A per-item or per-request deadline expired.
    #[error("operation timed out")]
    Timeout,

    /// Parallel range download failed.
    #[error("range download failed: {0}")]
    Range(String),

    /// One or more media items failed and partial results were not allowed.
    #[error("{} media item(s) failed", .0.len())]
    Partial(Vec<PartialFailure>),

    /// A blocking worker panicked or was cancelled.
    #[error("internal worker failure: {0}")]
    Internal(String),
}

impl YsttError {
    /// Stable machine-readable code for logs and HTTP APIs.
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidUrl(_) => "invalid_url",
            Self::Config(_) => "config",
            Self::UnsupportedUrl(_) => "unsupported_url",
            Self::Unavailable(_) => "unavailable",
            Self::AgeRestricted => "age_restricted",
            Self::Network(_) => "network",
            Self::Waf(_) => "waf",
            Self::Parse(_) => "parse",
            Self::NoMedia => "no_media",
            Self::NoVideo => "no_video",
            Self::FileTooLarge { .. } => "file_too_large",
            Self::FileExists(_) => "file_exists",
            Self::Io(_) => "io",
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
            Self::Range(_) => "range",
            Self::Partial(_) => "partial",
            Self::Internal(_) => "internal",
        }
    }

    /// Serialize this error as a small stable payload for HTTP APIs.
    pub fn info(&self) -> ErrorInfo {
        ErrorInfo {
            code: self.code(),
            message: self.to_string(),
            context: self.context(),
        }
    }

    fn context(&self) -> Option<ErrorContext> {
        let (url, status) = match self {
            Self::Network(NetError::Status { status, url }) => (Some(url.as_str()), Some(*status)),
            _ => (None, None),
        };
        let host = url
            .and_then(|url| url::Url::parse(url).ok())
            .and_then(|url| url.host_str().map(str::to_owned));
        let provider = host.as_deref().and_then(|host| {
            if host.ends_with("soundcloud.com") {
                Some("soundcloud".into())
            } else if host.contains("tiktok.com") {
                Some("tiktok".into())
            } else {
                None
            }
        });
        if host.is_none() && status.is_none() && provider.is_none() {
            return None;
        }
        Some(ErrorContext {
            provider,
            host,
            kind: None,
            status,
            attempt: None,
        })
    }

    /// Whether a retry at a higher layer can reasonably succeed.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Network(error) => error.is_retryable(),
            Self::Waf(WafError::NotSolved(_)) => false,
            Self::Io(_)
            | Self::Cancelled
            | Self::Timeout
            | Self::Parse(_)
            | Self::Unavailable(_)
            | Self::AgeRestricted
            | Self::NoMedia
            | Self::NoVideo
            | Self::InvalidUrl(_)
            | Self::Config(_)
            | Self::UnsupportedUrl(_)
            | Self::FileTooLarge { .. }
            | Self::FileExists(_)
            | Self::Range(_)
            | Self::Partial(_)
            | Self::Internal(_) => false,
            Self::Waf(_) => false,
        }
    }

    /// HTTP status code when the failure came from a response.
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Network(error) => error.status(),
            _ => None,
        }
    }
}

impl From<serde_json::Error> for YsttError {
    fn from(error: serde_json::Error) -> Self {
        Self::Parse(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_is_exposed_from_network_errors() {
        let error = YsttError::Network(NetError::Status {
            status: 403,
            url: "https://www.tiktok.com/x".into(),
        });
        assert_eq!(error.status(), Some(403));
        assert!(!error.is_retryable());
    }

    #[test]
    fn timeout_is_retryable() {
        assert!(YsttError::Network(NetError::Timeout).is_retryable());
    }

    #[test]
    fn network_error_context_has_host_status_and_provider() {
        let error = YsttError::Network(NetError::Status {
            status: 502,
            url: "https://api-v2.soundcloud.com/x".into(),
        });
        let info = error.info();
        let context = info.context.expect("context");
        assert_eq!(context.provider.as_deref(), Some("soundcloud"));
        assert_eq!(context.host.as_deref(), Some("api-v2.soundcloud.com"));
        assert_eq!(context.status, Some(502));
    }

    #[test]
    fn error_info_is_stable_and_serializable() {
        let error = YsttError::Unavailable("gone".into());
        assert_eq!(error.code(), "unavailable");
        let info = error.info();
        assert_eq!(info.code, "unavailable");
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains("\"code\":\"unavailable\""));
    }
}
