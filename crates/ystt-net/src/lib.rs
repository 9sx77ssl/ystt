//! HTTP transport and browser emulation for ystt.
//!
//! The transport is built on [`wreq`], a `reqwest` fork that preserves header
//! case and can emulate browser TLS/HTTP2 fingerprints. `ystt-net` owns no
//! TikTok-specific parsing; it exposes a small, typed request/response API.

mod client;
mod config;
mod error;
mod headers;
mod limiter;
mod response;
mod retry;

pub use client::{HttpClient, RequestClass, RequestOptions};
pub use config::{BrowserProfile, Http2Tuning, HttpClientConfig, TransportPolicy};
pub use error::NetError;
pub use headers::{BrowserHeaders, RequestKind};
pub use response::HttpResponse;
