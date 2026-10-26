//! Retry policy and backoff math.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Status codes that are safe to retry for idempotent requests.
pub(crate) const RETRYABLE_STATUS: [u16; 7] = [408, 425, 429, 500, 502, 503, 504];

/// Whether a status code is retryable.
pub(crate) fn is_retryable_status(status: u16) -> bool {
    RETRYABLE_STATUS.contains(&status)
}

/// Compute the delay before the next attempt.
///
/// `attempt` is zero-based. The base delay doubles per attempt, is capped at
/// `max`, and gets a small sub-second jitter so parallel clients do not
/// thunder together. A valid `Retry-After` seconds value wins when it is
/// larger than the computed backoff (still capped by `max`).
pub(crate) fn backoff(
    attempt: u32,
    base: Duration,
    max: Duration,
    retry_after: Option<&str>,
) -> Duration {
    let exponent = attempt.min(10);
    let delay = base.saturating_mul(1u32 << exponent).min(max);

    let jitter_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::from(elapsed.subsec_millis()) % 250)
        .unwrap_or(0);
    let with_jitter = delay
        .saturating_add(Duration::from_millis(jitter_ms))
        .min(max);

    retry_after
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .map(|value| value.min(max))
        .map(|value| value.max(with_jitter))
        .unwrap_or(with_jitter)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_statuses_are_limited() {
        for status in RETRYABLE_STATUS {
            assert!(is_retryable_status(status));
        }
        assert!(!is_retryable_status(200));
        assert!(!is_retryable_status(403));
        assert!(!is_retryable_status(404));
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        let base = Duration::from_millis(100);
        let max = Duration::from_secs(1);
        let first = backoff(0, base, max, None);
        let third = backoff(2, base, max, None);
        assert!(first <= third);
        assert!(third <= max);
    }

    #[test]
    fn retry_after_can_win_but_not_exceed_max() {
        let max = Duration::from_secs(5);
        assert_eq!(
            backoff(0, Duration::from_millis(10), max, Some("2")),
            Duration::from_secs(2)
        );
        assert_eq!(backoff(0, Duration::from_millis(10), max, Some("99")), max);
        let fallback = backoff(0, Duration::from_millis(10), max, Some("not-a-number"));
        assert!(fallback >= Duration::from_millis(10));
        assert!(fallback <= Duration::from_millis(260));
    }
}
