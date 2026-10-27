//! Small per-client token bucket for HTTP requests.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

struct Bucket {
    tokens: f64,
    updated: Instant,
}

/// Token-bucket limiter keyed by client address.
#[derive(Clone)]
pub struct RateLimiter {
    inner: Arc<Mutex<HashMap<String, Bucket>>>,
    rate_per_second: f64,
    burst: f64,
}

impl RateLimiter {
    /// Create a limiter with a refill rate and burst capacity.
    pub fn new(per_minute: u32, burst: u32) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            rate_per_second: f64::from(per_minute.max(1)) / 60.0,
            burst: f64::from(burst.max(1)),
        }
    }

    /// Consume one token for a client.
    pub fn allow(&self, key: &str) -> bool {
        let now = Instant::now();
        let mut buckets = self.inner.lock().expect("rate-limit lock");
        if buckets.len() > 10_000 {
            buckets.retain(|_, bucket| now.duration_since(bucket.updated).as_secs() < 120);
        }
        let bucket = buckets.entry(key.to_owned()).or_insert(Bucket {
            tokens: self.burst,
            updated: now,
        });
        let elapsed = now.duration_since(bucket.updated).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.rate_per_second).min(self.burst);
        bucket.updated = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burst_is_limited() {
        let limiter = RateLimiter::new(60, 2);
        assert!(limiter.allow("a"));
        assert!(limiter.allow("a"));
        assert!(!limiter.allow("a"));
        assert!(limiter.allow("b"));
    }
}
