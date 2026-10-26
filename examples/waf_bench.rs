//! Offline WAF solver benchmark.
//!
//! Run:
//!   cargo run --release --example waf_bench
//!
//! Ten fixed nonces are solved for each thread count. Averaging over several
//! positions avoids reporting a lucky or unlucky single nonce as a scaling
//! result.

use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::json;
use ystt::{WafChallenge, WafSolver, WafSolverConfig, candidate_digest};

const NONCES: [u64; 10] = [
    12_345, 98_765, 234_567, 333_333, 456_789, 555_555, 654_321, 777_777, 876_543, 987_654,
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let prefix = b"ystt-waf-benchmark";
    let challenges: Vec<WafChallenge> = NONCES
        .iter()
        .map(|nonce| challenge(prefix, *nonce))
        .collect::<Result<_, _>>()?;

    println!(
        "nonces={} candidates=1_000_001 cores={}",
        NONCES.len(),
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0)
    );

    for threads in [1usize, 2, 4, 8] {
        let solver = WafSolver::new(WafSolverConfig {
            threads: Some(threads),
            max_iterations: 1_000_000,
        });

        let mut total = Duration::ZERO;
        for challenge in &challenges {
            let started = Instant::now();
            let _cookie = solver.solve(challenge)?;
            total += started.elapsed();
        }

        let average = total / NONCES.len() as u32;
        println!(
            "threads={threads:<2} average={:>8.2} ms total={:>8.2} ms",
            average.as_secs_f64() * 1000.0,
            total.as_secs_f64() * 1000.0
        );
    }

    Ok(())
}

fn challenge(prefix: &[u8], nonce: u64) -> Result<WafChallenge, Box<dyn std::error::Error>> {
    let expected = candidate_digest(prefix, nonce);
    let payload = json!({
        "v": {
            "a": STANDARD.encode(prefix),
            "c": STANDARD.encode(expected),
        },
        "d": "",
    });
    let encoded = STANDARD.encode(serde_json::to_vec(&payload)?);
    Ok(WafChallenge::from_encoded(
        &encoded,
        "waf_cookie",
        1_000_000,
    )?)
}
