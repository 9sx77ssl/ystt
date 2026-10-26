//! `ystt-waf` — TikTok WAF challenge parsing and proof-of-work solving.
//!
//! This crate is deliberately independent of HTTP and of the rest of `ystt`.
//! It can be used on its own to solve a challenge captured from any client.

mod challenge;
mod solver;

pub use challenge::{WafChallenge, WafError, candidate_digest, looks_like_external_bot_page};
pub use solver::{WafSolver, WafSolverConfig, solve_challenge, solve_nonce_parallel};
