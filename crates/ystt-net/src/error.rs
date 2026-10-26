//! Typed transport errors.

use thiserror::Error;

/// Errors produced by the HTTP layer.
#[derive(Debug, Error)]
pub enum NetError {
    /// Request or body read exceeded the timeout.
    #[error("request timed out")]
    Timeout,

    /// TCP or TLS connection failed.
    #[error("connection failed: {0}")]
    Connect(String),

    /// Request construction or transfer failed.
    #[error("request failed: {0}")]
    Request(String),

    /// Response body could not be read or decoded.
    #[error("body read failed: {0}")]
    Body(String),

    /// Server returned a non-success status.
    #[error("HTTP {status} for {url}")]
    Status {
        /// HTTP status.
        status: u16,
        /// Request URL.
        url: String,
    },

    /// Response exceeded the configured byte cap.
    #[error("response exceeds the {limit} byte cap")]
    TooLarge {
        /// Byte cap.
        limit: u64,
    },

    /// Operation was cancelled by the caller.
    #[error("cancelled")]
    Cancelled,

    /// A header value could not be constructed.
    #[error("invalid header: {0}")]
    Header(String),

    /// Client construction failed.
    #[error("client build failed: {0}")]
    Build(String),
}

impl NetError {
    /// Whether a retry can reasonably succeed.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Timeout | Self::Connect(_) | Self::Request(_) | Self::Body(_) => true,
            Self::Status { status, .. } => super::retry::is_retryable_status(*status),
            Self::TooLarge { .. } | Self::Cancelled | Self::Header(_) | Self::Build(_) => false,
        }
    }

    /// HTTP status when the error came from a response.
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Status { status, .. } => Some(*status),
            _ => None,
        }
    }
}

pub(crate) fn map_wreq_error(error: wreq::Error) -> NetError {
    if error.is_timeout() {
        NetError::Timeout
    } else if error.is_connect() {
        NetError::Connect(error.to_string())
    } else if error.is_body() || error.is_decode() {
        NetError::Body(error.to_string())
    } else {
        NetError::Request(error.to_string())
    }
}
