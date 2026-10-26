//! Multi-threaded proof-of-work solver for [`WafChallenge`].
//!
//! The search is embarrassingly parallel: every thread owns a contiguous
//! range of nonces and clones a pre-initialized SHA-256 state. A shared
//! atomic flag stops the other threads shortly after a solution is found.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;

use sha2::{Digest, Sha256};
use tracing::debug;

use crate::challenge::{WafChallenge, WafError};

/// Tunable solver configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WafSolverConfig {
    /// Worker threads. `None` means `std::thread::available_parallelism()`.
    pub threads: Option<usize>,
    /// Hard cap on the nonce search, independent of the challenge value.
    pub max_iterations: u64,
}

impl Default for WafSolverConfig {
    fn default() -> Self {
        Self {
            threads: None,
            max_iterations: 2_000_000,
        }
    }
}

/// Stateless WAF solver.
#[derive(Debug, Clone, Default)]
pub struct WafSolver {
    config: WafSolverConfig,
}

impl WafSolver {
    /// Create a solver with explicit configuration.
    pub fn new(config: WafSolverConfig) -> Self {
        Self { config }
    }

    /// Solve a parsed challenge and return the `name=value` cookie.
    pub fn solve(&self, challenge: &WafChallenge) -> Result<String, WafError> {
        let max = challenge.max_iterations.min(self.config.max_iterations);
        let nonce = self.solve_nonce(&challenge.prefix, &challenge.expected, max)?;
        debug!(nonce, "WAF challenge solved");
        challenge.cookie_for(nonce)
    }

    /// Solve only the nonce. Useful for tests and external integrations.
    pub fn solve_nonce(
        &self,
        prefix: &[u8],
        expected: &[u8; 32],
        max_iterations: u64,
    ) -> Result<u64, WafError> {
        let threads = resolve_threads(self.config.threads);
        solve_nonce_parallel(prefix, expected, max_iterations, threads)
    }
}

/// Solve a single challenge without constructing a [`WafSolver`].
pub fn solve_challenge(
    challenge: &WafChallenge,
    config: WafSolverConfig,
) -> Result<String, WafError> {
    WafSolver::new(config).solve(challenge)
}

/// Parallel brute-force for `SHA256(prefix || decimal(nonce)) == expected`.
///
/// Nonces are partitioned into `threads` contiguous ranges. Each thread clones
/// a SHA-256 state that already contains `prefix`, so only the decimal digits
/// are hashed per candidate.
pub fn solve_nonce_parallel(
    prefix: &[u8],
    expected: &[u8; 32],
    max_iterations: u64,
    threads: usize,
) -> Result<u64, WafError> {
    let workers = threads.max(1);
    let total = max_iterations.saturating_add(1);
    let chunk = total.div_ceil(workers as u64);

    let found = AtomicU64::new(u64::MAX);
    let stop = AtomicBool::new(false);

    thread::scope(|scope| {
        for worker in 0..workers {
            let start = (worker as u64).saturating_mul(chunk);
            if start >= total {
                break;
            }
            let end = start.saturating_add(chunk).min(total);

            let found = &found;
            let stop = &stop;

            scope.spawn(move || {
                let mut base = Sha256::new();
                base.update(prefix);
                let mut number = itoa::Buffer::new();

                for nonce in start..end {
                    // A relaxed check every 1024 candidates is cheap and keeps
                    // the other workers from running long after a solution.
                    if nonce & 0x3ff == 0 && stop.load(Ordering::Relaxed) {
                        return;
                    }

                    let mut hash = base.clone();
                    hash.update(number.format(nonce).as_bytes());
                    if hash.finalize().as_slice() == expected.as_slice() {
                        found.store(nonce, Ordering::Relaxed);
                        stop.store(true, Ordering::Relaxed);
                        return;
                    }
                }
            });
        }
    });

    let nonce = found.load(Ordering::Relaxed);
    if nonce == u64::MAX {
        Err(WafError::NotSolved(max_iterations))
    } else {
        Ok(nonce)
    }
}

fn resolve_threads(configured: Option<usize>) -> usize {
    configured
        .filter(|threads| *threads > 0)
        .unwrap_or_else(|| {
            thread::available_parallelism()
                .map(|threads| threads.get())
                .unwrap_or(4)
        })
        .clamp(1, 64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::challenge::candidate_digest;
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use serde_json::json;

    #[test]
    fn sha256_known_answer() {
        let digest = candidate_digest(b"abc", 0);
        // This is SHA256(b"abc0"), not SHA256(b"abc"). Verify our construction
        // against an independent digest of the concatenated bytes.
        let mut independent = Sha256::new();
        independent.update(b"abc0");
        assert_eq!(digest, independent.finalize().as_slice());
    }

    #[test]
    fn solves_known_nonce_parallel() {
        let prefix = b"hello-world";
        let nonce = 4321u64;
        let expected = candidate_digest(prefix, nonce);

        let result = solve_nonce_parallel(prefix, &expected, 10_000, 4).unwrap();
        assert_eq!(result, nonce);
    }

    #[test]
    fn reports_not_solved_when_budget_is_too_small() {
        let prefix = b"prefix";
        let expected = candidate_digest(prefix, 999);
        let error = solve_nonce_parallel(prefix, &expected, 100, 2).unwrap_err();
        assert_eq!(error, WafError::NotSolved(100));
    }

    #[test]
    fn single_thread_and_multi_thread_agree() {
        let prefix = b"agreement";
        let expected = candidate_digest(prefix, 77);
        assert_eq!(
            solve_nonce_parallel(prefix, &expected, 1_000, 1).unwrap(),
            77
        );
        assert_eq!(
            solve_nonce_parallel(prefix, &expected, 1_000, 8).unwrap(),
            77
        );
    }

    #[test]
    fn solver_returns_a_valid_cookie() {
        let prefix = b"cookie";
        let nonce = 123u64;
        let expected = candidate_digest(prefix, nonce);
        let payload = json!({
            "v": {"a": STANDARD.encode(prefix), "c": STANDARD.encode(expected)},
            "d": "",
        });
        let encoded = STANDARD.encode(serde_json::to_vec(&payload).unwrap());
        let challenge = WafChallenge::from_encoded(&encoded, "waf_cookie", 1_000).unwrap();

        let cookie = WafSolver::default().solve(&challenge).unwrap();
        assert!(cookie.starts_with("waf_cookie="));

        let raw = STANDARD
            .decode(cookie.trim_start_matches("waf_cookie="))
            .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(value["d"], STANDARD.encode(nonce.to_string()));
    }
}
