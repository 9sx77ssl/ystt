//! HTTP client configuration and browser profiles.

use std::time::Duration;

use wreq_util::{Emulation, Platform, Profile};

/// HTTP protocol preference for the transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransportPolicy {
    /// Prefer HTTP/2 via ALPN and fall back to HTTP/1.1 when the CDN supports
    /// only HTTP/1.1. This is the default and supports every TikTok CDN host.
    #[default]
    Auto,
    /// Require HTTP/2 for every request.
    Http2Only,
    /// Force HTTP/1.1 for every request.
    Http1Only,
}

impl TransportPolicy {
    /// Stable name for logs and configuration.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Http2Only => "http2",
            Self::Http1Only => "http1",
        }
    }
}

impl std::str::FromStr for TransportPolicy {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" | "alpn" => Ok(Self::Auto),
            "http2" | "h2" => Ok(Self::Http2Only),
            "http1" | "h1" => Ok(Self::Http1Only),
            other => Err(format!("unknown transport policy: {other}")),
        }
    }
}

/// Browser profile used for TLS and HTTP/2 emulation.
///
/// The list intentionally exposes a few well-known profiles rather than all
/// 100+ profiles shipped by `wreq-util`. Add a variant when a new benchmark
/// shows a concrete benefit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BrowserProfile {
    /// Latest Chrome profile in the pinned `wreq-util` release.
    #[default]
    Chrome149,
    /// Slightly older Chrome profile, useful as a fallback.
    Chrome136,
    /// Chrome 131, the profile used by several anti-bot test suites.
    Chrome131,
    /// Latest Firefox profile.
    Firefox151,
    /// Latest Safari profile.
    Safari26_4,
}

impl BrowserProfile {
    /// Map the profile to a `wreq-util` emulation preset.
    pub fn emulation(self) -> Emulation {
        let (profile, platform) = match self {
            Self::Chrome149 => (Profile::Chrome149, Platform::Windows),
            Self::Chrome136 => (Profile::Chrome136, Platform::Windows),
            Self::Chrome131 => (Profile::Chrome131, Platform::Windows),
            Self::Firefox151 => (Profile::Firefox151, Platform::Windows),
            Self::Safari26_4 => (Profile::Safari26_4, Platform::MacOS),
        };
        Emulation::builder()
            .profile(profile)
            .platform(platform)
            .build()
    }

    /// Stable human-readable name for logs and CLI output.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chrome149 => "chrome_149",
            Self::Chrome136 => "chrome_136",
            Self::Chrome131 => "chrome_131",
            Self::Firefox151 => "firefox_151",
            Self::Safari26_4 => "safari_26.4",
        }
    }
}

impl std::str::FromStr for BrowserProfile {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "chrome149" | "chrome_149" | "chrome" => Ok(Self::Chrome149),
            "chrome136" | "chrome_136" => Ok(Self::Chrome136),
            "chrome131" | "chrome_131" => Ok(Self::Chrome131),
            "firefox151" | "firefox_151" | "firefox" => Ok(Self::Firefox151),
            "safari26_4" | "safari" => Ok(Self::Safari26_4),
            other => Err(format!("unknown browser profile: {other}")),
        }
    }
}

/// Configuration for [`crate::HttpClient`].
#[derive(Debug, Clone)]
pub struct HttpClientConfig {
    /// Total request timeout including connection and response headers.
    pub timeout: Duration,
    /// TCP/TLS connection timeout.
    pub connect_timeout: Duration,
    /// Idle read timeout while streaming a body.
    pub read_timeout: Duration,
    /// How long idle pooled connections are kept.
    pub pool_idle_timeout: Duration,
    /// Retries for idempotent requests (GET/HEAD).
    pub retries: u32,
    /// Base delay for exponential backoff.
    pub retry_base_delay: Duration,
    /// Maximum delay between retries.
    pub retry_max_delay: Duration,
    /// Maximum concurrent page requests per host.
    pub per_host_concurrency: usize,
    /// Minimum spacing between page requests to one host.
    pub min_request_interval: Duration,
    /// Browser fingerprint profile.
    pub browser: BrowserProfile,
    /// Optional user-agent override. `None` uses the emulation profile.
    pub user_agent: Option<String>,
    /// Disable environment/system proxies. `true` by default for ystt.
    pub no_proxy: bool,
    /// HTTP protocol preference. `Auto` negotiates H2 and falls back to H1.
    pub transport: TransportPolicy,
}

impl Default for HttpClientConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(20),
            connect_timeout: Duration::from_secs(10),
            read_timeout: Duration::from_secs(60),
            pool_idle_timeout: Duration::from_secs(90),
            retries: 2,
            retry_base_delay: Duration::from_millis(250),
            retry_max_delay: Duration::from_secs(8),
            per_host_concurrency: 2,
            min_request_interval: Duration::from_millis(120),
            browser: BrowserProfile::default(),
            user_agent: None,
            no_proxy: true,
            transport: TransportPolicy::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_names_are_stable() {
        assert_eq!(BrowserProfile::Chrome149.as_str(), "chrome_149");
        assert_eq!(BrowserProfile::Firefox151.as_str(), "firefox_151");
    }

    #[test]
    fn transport_policy_parses_stably() {
        assert_eq!(
            "auto".parse::<TransportPolicy>().unwrap(),
            TransportPolicy::Auto
        );
        assert_eq!(
            "h2".parse::<TransportPolicy>().unwrap(),
            TransportPolicy::Http2Only
        );
        assert_eq!(
            "http1".parse::<TransportPolicy>().unwrap(),
            TransportPolicy::Http1Only
        );
        assert_eq!(TransportPolicy::Auto.as_str(), "auto");
    }

    #[test]
    fn defaults_are_conservative() {
        let config = HttpClientConfig::default();
        assert_eq!(config.retries, 2);
        assert!(config.no_proxy);
        assert_eq!(config.per_host_concurrency, 2);
        assert_eq!(config.transport, TransportPolicy::Auto);
    }
}
