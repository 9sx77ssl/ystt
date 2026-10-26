//! Transfer metrics for pages and media CDN responses.

use serde::Serialize;

/// Kind of transfer represented by a metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TransferKind {
    /// HTML page or embed document.
    Page,
    /// Video stream.
    Video,
    /// Gallery image.
    Photo,
    /// Music/audio stream.
    Audio,
}

/// One completed network transfer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TransferMetric {
    /// CDN or TikTok host.
    pub host: String,
    /// Transfer kind.
    pub kind: TransferKind,
    /// Gallery index for photos.
    pub index: Option<usize>,
    /// Negotiated protocol, e.g. `HTTP/2.0` or `HTTP/1.1`.
    pub protocol: String,
    /// Bytes transferred.
    pub bytes: u64,
    /// Wall time from request start through body completion.
    pub elapsed_ms: u64,
    /// Derived throughput in bits per second.
    pub throughput_bps: u64,
}

impl TransferMetric {
    /// Build a metric from a URL, protocol, size and elapsed time.
    pub fn new(
        url: &str,
        kind: TransferKind,
        index: Option<usize>,
        protocol: &str,
        bytes: u64,
        elapsed: std::time::Duration,
    ) -> Self {
        let elapsed_ms = elapsed.as_millis().min(u128::from(u64::MAX)) as u64;
        let throughput_bps = bytes
            .saturating_mul(8)
            .saturating_mul(1000)
            .checked_div(elapsed_ms)
            .unwrap_or(0);
        Self {
            host: host_of(url),
            kind,
            index,
            protocol: protocol.to_owned(),
            bytes,
            elapsed_ms,
            throughput_bps,
        }
    }
}

fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn metric_computes_throughput_and_host() {
        let metric = TransferMetric::new(
            "https://v16.example/v.mp4",
            TransferKind::Video,
            None,
            "HTTP/2.0",
            1_000_000,
            Duration::from_millis(500),
        );
        assert_eq!(metric.host, "v16.example");
        assert_eq!(metric.throughput_bps, 16_000_000);
        assert_eq!(metric.protocol, "HTTP/2.0");
    }
}
