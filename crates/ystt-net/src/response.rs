//! Typed response wrapper with bounded, cancellable body reading.

use std::time::Duration;

use bytes::{Bytes, BytesMut};
use futures_util::{Stream, StreamExt};
use tokio_util::sync::CancellationToken;
use wreq::StatusCode;
use wreq::header::{CONTENT_LENGTH, HeaderMap, SET_COOKIE};

use crate::NetError;
use crate::error::map_wreq_error;

/// A response plus the request URL, so error messages stay useful.
pub struct HttpResponse {
    inner: wreq::Response,
    url: String,
}

impl HttpResponse {
    pub(crate) fn new(inner: wreq::Response, url: String) -> Self {
        Self { inner, url }
    }

    /// URL that produced this response.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Status code.
    pub fn status(&self) -> StatusCode {
        self.inner.status()
    }

    /// Status code as `u16`.
    pub fn status_u16(&self) -> u16 {
        self.inner.status().as_u16()
    }

    /// HTTP protocol version negotiated with the server.
    pub fn version(&self) -> wreq::Version {
        self.inner.version()
    }

    /// Whether the status is 2xx.
    pub fn is_success(&self) -> bool {
        self.inner.status().is_success()
    }

    /// Response headers.
    pub fn headers(&self) -> &HeaderMap {
        self.inner.headers()
    }

    /// Declared body length, when the server sent `Content-Length`.
    pub fn content_length(&self) -> Option<u64> {
        self.inner
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
            .or_else(|| self.inner.content_length())
    }

    /// All `Set-Cookie` header values, in order.
    pub fn set_cookie_headers(&self) -> Vec<String> {
        self.inner
            .headers()
            .get_all(SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok().map(str::to_owned))
            .collect()
    }

    /// Read the full body as UTF-8 with lossy replacement.
    pub async fn text(
        self,
        max_bytes: u64,
        read_timeout: Duration,
        signal: Option<&CancellationToken>,
    ) -> Result<String, NetError> {
        let bytes = self.bytes(max_bytes, read_timeout, signal).await?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Read the full body, enforcing a byte cap and idle timeout.
    pub async fn bytes(
        self,
        max_bytes: u64,
        read_timeout: Duration,
        signal: Option<&CancellationToken>,
    ) -> Result<Bytes, NetError> {
        if let Some(length) = self.content_length()
            && length > max_bytes
        {
            return Err(NetError::TooLarge { limit: max_bytes });
        }
        collect_stream(self.inner.bytes_stream(), max_bytes, read_timeout, signal).await
    }

    /// Consume the response into a byte stream.
    ///
    /// The caller is responsible for byte caps, idle timeouts and writing.
    pub fn into_stream(self) -> impl Stream<Item = Result<Bytes, NetError>> {
        self.inner
            .bytes_stream()
            .map(|item| item.map_err(map_wreq_error))
    }
}

async fn collect_stream<S>(
    stream: S,
    max_bytes: u64,
    read_timeout: Duration,
    signal: Option<&CancellationToken>,
) -> Result<Bytes, NetError>
where
    S: Stream<Item = wreq::Result<Bytes>>,
{
    let mut stream = Box::pin(stream);
    let mut buffer = BytesMut::new();

    loop {
        let next = tokio::time::timeout(read_timeout, stream.next());
        let item = match signal {
            Some(signal) => {
                tokio::select! {
                    _ = signal.cancelled() => return Err(NetError::Cancelled),
                    result = next => result,
                }
            }
            None => next.await,
        };

        match item {
            Err(_) => return Err(NetError::Timeout),
            Ok(None) => break,
            Ok(Some(Err(error))) => return Err(map_wreq_error(error)),
            Ok(Some(Ok(chunk))) => {
                if buffer.len() as u64 + chunk.len() as u64 > max_bytes {
                    return Err(NetError::TooLarge { limit: max_bytes });
                }
                buffer.extend_from_slice(&chunk);
            }
        }
    }

    Ok(buffer.freeze())
}
