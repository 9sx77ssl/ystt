//! Per-host concurrency and spacing limiter.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::NetError;

#[derive(Debug)]
struct HostState {
    semaphore: Arc<Semaphore>,
    next_start: Mutex<Instant>,
}

/// Limits page requests per host without throttling media CDN transfers.
#[derive(Debug)]
pub(crate) struct HostLimiter {
    concurrency: usize,
    interval: Duration,
    hosts: Mutex<HashMap<String, Arc<HostState>>>,
}

impl HostLimiter {
    pub(crate) fn new(concurrency: usize, interval: Duration) -> Self {
        Self {
            concurrency: concurrency.max(1),
            interval,
            hosts: Mutex::new(HashMap::new()),
        }
    }

    /// Acquire a permit and wait until the host is allowed to start.
    pub(crate) async fn acquire(&self, host: &str) -> Result<OwnedSemaphorePermit, NetError> {
        let state = {
            let mut hosts = self
                .hosts
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            hosts
                .entry(host.to_owned())
                .or_insert_with(|| {
                    Arc::new(HostState {
                        semaphore: Arc::new(Semaphore::new(self.concurrency)),
                        next_start: Mutex::new(Instant::now()),
                    })
                })
                .clone()
        };

        let permit = state
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| NetError::Cancelled)?;

        let wait = {
            let mut next_start = state
                .next_start
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let now = Instant::now();
            let start = (*next_start).max(now);
            *next_start = start + self.interval;
            start.saturating_duration_since(now)
        };

        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }

        Ok(permit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn permits_are_released() {
        let limiter = HostLimiter::new(1, Duration::from_millis(1));
        let first = limiter.acquire("example.com").await.unwrap();
        drop(first);
        let _second = limiter.acquire("example.com").await.unwrap();
    }

    #[tokio::test]
    async fn spacing_is_applied() {
        let limiter = HostLimiter::new(4, Duration::from_millis(20));
        let started = Instant::now();
        let _a = limiter.acquire("spacing.test").await.unwrap();
        drop(_a);
        let _b = limiter.acquire("spacing.test").await.unwrap();
        assert!(started.elapsed() >= Duration::from_millis(20));
    }
}
