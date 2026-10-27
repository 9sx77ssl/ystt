//! The `wreq`-backed HTTP client.

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing::debug;
use wreq::header::{HeaderMap, RETRY_AFTER, USER_AGENT};
use wreq::redirect::Policy;
use wreq::{Client, IntoEmulation, Method};

use crate::NetError;
use crate::config::{Http2Tuning, HttpClientConfig, TransportPolicy};
use crate::error::map_wreq_error;
use crate::limiter::HostLimiter;
use crate::response::HttpResponse;
use crate::retry::{backoff, is_retryable_status};

/// Class of request, used to decide whether rate limiting applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestClass {
    /// TikTok HTML page.
    Page,
    /// TikTok short-link redirect page.
    ShortLink,
    /// Media CDN transfer.
    Media,
}

impl RequestClass {
    fn is_rate_limited(self) -> bool {
        matches!(self, Self::Page | Self::ShortLink)
    }
}

/// Per-request overrides.
#[derive(Debug, Clone)]
pub struct RequestOptions {
    /// Request class.
    pub class: RequestClass,
    /// Override the configured retry count.
    pub retries: Option<u32>,
    /// Override the total request timeout.
    pub timeout: Option<Duration>,
    /// Override the body idle timeout.
    pub read_timeout: Option<Duration>,
    /// Cancellation token.
    pub signal: Option<CancellationToken>,
    /// Follow redirects.
    pub allow_redirects: bool,
    /// Maximum redirects when following.
    pub max_redirects: usize,
}

impl Default for RequestOptions {
    fn default() -> Self {
        Self {
            class: RequestClass::Page,
            retries: None,
            timeout: None,
            read_timeout: None,
            signal: None,
            allow_redirects: true,
            max_redirects: 5,
        }
    }
}

impl RequestOptions {
    /// A normal page navigation request.
    pub fn page() -> Self {
        Self::default()
    }

    /// A short-link request that must not follow redirects automatically.
    pub fn short_link() -> Self {
        Self {
            class: RequestClass::ShortLink,
            allow_redirects: false,
            ..Self::default()
        }
    }

    /// A media request.
    pub fn media() -> Self {
        Self {
            class: RequestClass::Media,
            ..Self::default()
        }
    }

    /// Override retries.
    pub fn with_retries(mut self, retries: u32) -> Self {
        self.retries = Some(retries);
        self
    }

    /// Override the total timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Attach a cancellation token.
    pub fn with_signal(mut self, signal: CancellationToken) -> Self {
        self.signal = Some(signal);
        self
    }
}

/// Cheap-to-clone HTTP client sharing one connection pool.
#[derive(Clone)]
pub struct HttpClient {
    inner: Client,
    config: HttpClientConfig,
    limiter: Arc<HostLimiter>,
}

impl std::fmt::Debug for HttpClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HttpClient")
            .field("browser", &self.config.browser)
            .field("retries", &self.config.retries)
            .finish_non_exhaustive()
    }
}

impl HttpClient {
    /// Build a client using browser emulation and the configured pool limits.
    pub fn new(config: HttpClientConfig) -> Result<Self, NetError> {
        let mut emulation = config.browser.emulation().into_emulation();
        tune_http2(&mut emulation, &config.http2);
        let mut builder = Client::builder()
            .emulation(emulation)
            .timeout(config.timeout)
            .connect_timeout(config.connect_timeout)
            .read_timeout(config.read_timeout)
            .pool_idle_timeout(config.pool_idle_timeout)
            .pool_max_idle_per_host(config.pool_max_idle_per_host)
            .tcp_nodelay(true)
            .gzip(true)
            .brotli(true)
            .deflate(true)
            .zstd(true);

        if config.no_proxy {
            builder = builder.no_proxy();
        }
        // Production requires HTTPS. The feature-gated local integration
        // harness is plain HTTP on 127.0.0.1, so only that build relaxes this.
        #[cfg(not(feature = "integration-tests"))]
        {
            builder = builder.https_only(true);
        }
        match config.transport {
            TransportPolicy::Auto => {}
            TransportPolicy::Http2Only => {
                builder = builder.http2_only();
            }
            TransportPolicy::Http1Only => {
                builder = builder.http1_only();
            }
        }

        let inner = builder
            .build()
            .map_err(|error| NetError::Build(error.to_string()))?;
        let limiter = Arc::new(HostLimiter::new(
            config.per_host_concurrency,
            config.min_request_interval,
        ));

        Ok(Self {
            inner,
            config,
            limiter,
        })
    }

    /// Effective configuration.
    pub fn config(&self) -> &HttpClientConfig {
        &self.config
    }

    /// Send a GET request.
    pub async fn get(
        &self,
        url: &str,
        headers: HeaderMap,
        options: RequestOptions,
    ) -> Result<HttpResponse, NetError> {
        self.execute(Method::GET, url, headers, options).await
    }

    /// Send a HEAD request.
    pub async fn head(
        &self,
        url: &str,
        headers: HeaderMap,
        options: RequestOptions,
    ) -> Result<HttpResponse, NetError> {
        self.execute(Method::HEAD, url, headers, options).await
    }

    async fn execute(
        &self,
        method: Method,
        url: &str,
        headers: HeaderMap,
        options: RequestOptions,
    ) -> Result<HttpResponse, NetError> {
        let retries = options.retries.unwrap_or(self.config.retries);
        let timeout = options.timeout.unwrap_or(self.config.timeout);
        let read_timeout = options.read_timeout.unwrap_or(self.config.read_timeout);
        let host = host_of(url);
        let mut last_error: Option<NetError> = None;

        for attempt in 0..=retries {
            if let Some(signal) = &options.signal
                && signal.is_cancelled()
            {
                return Err(NetError::Cancelled);
            }

            let _permit = if options.class.is_rate_limited() {
                Some(self.limiter.acquire(&host).await?)
            } else {
                None
            };

            let mut builder = self.inner.request(method.clone(), url);
            for (name, value) in headers.iter() {
                builder = builder.header(name.clone(), value.clone());
            }
            if let Some(user_agent) = &self.config.user_agent {
                builder = builder.header(USER_AGENT, user_agent);
            }

            builder = builder
                .timeout(timeout)
                .read_timeout(read_timeout)
                .redirect(if options.allow_redirects {
                    Policy::limited(options.max_redirects)
                } else {
                    Policy::none()
                });

            let send = builder.send();
            let result = match &options.signal {
                Some(signal) => {
                    tokio::select! {
                        _ = signal.cancelled() => return Err(NetError::Cancelled),
                        result = send => result,
                    }
                }
                None => send.await,
            };

            match result {
                Ok(response) => {
                    let status = response.status().as_u16();
                    if attempt < retries && is_retryable_status(status) {
                        let retry_after = response
                            .headers()
                            .get(RETRY_AFTER)
                            .and_then(|value| value.to_str().ok())
                            .map(str::to_owned);
                        drop(response);

                        let delay = backoff(
                            attempt,
                            self.config.retry_base_delay,
                            self.config.retry_max_delay,
                            retry_after.as_deref(),
                        );
                        debug!(attempt, status, ?delay, "retrying HTTP request");
                        sleep_or_cancel(delay, options.signal.as_ref()).await?;
                        continue;
                    }

                    return Ok(HttpResponse::new(response, url.to_owned()));
                }
                Err(error) => {
                    let mapped = map_wreq_error(error);
                    if attempt < retries && mapped.is_retryable() {
                        let delay = backoff(
                            attempt,
                            self.config.retry_base_delay,
                            self.config.retry_max_delay,
                            None,
                        );
                        debug!(attempt, ?mapped, ?delay, "retrying after transport error");
                        last_error = Some(mapped);
                        sleep_or_cancel(delay, options.signal.as_ref()).await?;
                        continue;
                    }
                    return Err(mapped);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| NetError::Request("retry loop exhausted".into())))
    }
}

async fn sleep_or_cancel(
    delay: Duration,
    signal: Option<&CancellationToken>,
) -> Result<(), NetError> {
    match signal {
        Some(signal) => {
            tokio::select! {
                _ = signal.cancelled() => Err(NetError::Cancelled),
                _ = tokio::time::sleep(delay) => Ok(()),
            }
        }
        None => {
            tokio::time::sleep(delay).await;
            Ok(())
        }
    }
}

fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_else(|| "invalid-host".into())
}

fn tune_http2(emulation: &mut wreq::Emulation, tuning: &Http2Tuning) {
    let Some(http2) = emulation.http2_options.as_mut() else {
        return;
    };
    if let Some(window) = tuning.connection_window {
        http2.initial_conn_window_size = window;
    }
    if let Some(interval) = tuning.keep_alive_interval {
        http2.keep_alive_interval = Some(interval);
    }
    if let Some(timeout) = tuning.keep_alive_timeout {
        http2.keep_alive_timeout = timeout;
    }
    if let Some(while_idle) = tuning.keep_alive_while_idle {
        http2.keep_alive_while_idle = while_idle;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_is_extracted() {
        assert_eq!(
            host_of("https://www.tiktok.com/@u/video/1"),
            "www.tiktok.com"
        );
        assert_eq!(host_of("not a url"), "invalid-host");
    }

    #[test]
    fn short_links_do_not_follow_redirects() {
        let options = RequestOptions::short_link();
        assert!(!options.allow_redirects);
        assert_eq!(options.class, RequestClass::ShortLink);
    }

    #[test]
    fn http2_tuning_keeps_profile_settings() {
        let mut emulation = crate::config::BrowserProfile::Chrome149
            .emulation()
            .into_emulation();
        let before = emulation.http2_options.as_ref().unwrap().clone();
        tune_http2(
            &mut emulation,
            &Http2Tuning {
                connection_window: Some(64 * 1024 * 1024),
                keep_alive_interval: Some(Duration::from_secs(30)),
                keep_alive_timeout: Some(Duration::from_secs(5)),
                keep_alive_while_idle: Some(true),
            },
        );
        let after = emulation.http2_options.as_ref().unwrap();
        assert_eq!(after.initial_window_size, before.initial_window_size);
        assert_eq!(after.max_frame_size, before.max_frame_size);
        assert_eq!(after.initial_conn_window_size, 64 * 1024 * 1024);
        assert_eq!(after.keep_alive_interval, Some(Duration::from_secs(30)));
        assert_eq!(after.keep_alive_timeout, Duration::from_secs(5));
        assert!(after.keep_alive_while_idle);
    }
}
