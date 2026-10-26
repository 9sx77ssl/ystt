//! Offline WAF parser and solver tests.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::json;
use ystt::{WafChallenge, WafError, WafSolver, WafSolverConfig, candidate_digest};

fn encoded_challenge(prefix: &[u8], nonce: u64) -> String {
    let expected = candidate_digest(prefix, nonce);
    let payload = json!({
        "v": {
            "a": STANDARD.encode(prefix),
            "c": STANDARD.encode(expected),
        },
        "d": "",
        "keep": {"nested": true},
    });
    STANDARD.encode(serde_json::to_vec(&payload).unwrap())
}

fn html_for(prefix: &[u8], nonce: u64) -> String {
    let encoded = encoded_challenge(prefix, nonce);
    format!(
        r#"<html><body><p id="cs" class="{encoded}"></p><p id="wci" class="waf_cookie"></p></body></html>"#
    )
}

#[test]
fn parses_and_solves_a_known_challenge() {
    let prefix = b"ystt-offline";
    let nonce = 4321;
    let html = html_for(prefix, nonce);

    let challenge = WafChallenge::parse_html(&html, 10_000)
        .unwrap()
        .expect("challenge present");
    assert_eq!(challenge.cookie_name, "waf_cookie");
    assert_eq!(challenge.expected, candidate_digest(prefix, nonce));

    let solver = WafSolver::new(WafSolverConfig {
        threads: Some(4),
        max_iterations: 10_000,
    });
    let cookie = solver.solve(&challenge).unwrap();
    assert!(cookie.starts_with("waf_cookie="));

    let raw = STANDARD
        .decode(cookie.trim_start_matches("waf_cookie="))
        .unwrap();
    let payload: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(payload["d"], STANDARD.encode(nonce.to_string()));
    assert_eq!(payload["keep"]["nested"], true);
}

#[test]
fn reports_not_solved_when_budget_is_too_small() {
    let prefix = b"ystt-offline";
    let nonce = 9000;
    let html = html_for(prefix, nonce);
    let challenge = WafChallenge::parse_html(&html, 100).unwrap().unwrap();

    let error = WafSolver::default()
        .solve(&challenge)
        .expect_err("must not solve");
    assert!(matches!(error, WafError::NotSolved(_)));
}

#[test]
fn tabs_and_attribute_order_are_tolerated() {
    let prefix = b"tolerant";
    let encoded = encoded_challenge(prefix, 7);
    let html =
        format!("<p class='{encoded}' data-x=\"1\" id='cs'></p><p class='waf' id='wci'></p>");
    let challenge = WafChallenge::parse_html(&html, 100)
        .unwrap()
        .expect("challenge present");
    assert_eq!(challenge.cookie_name, "waf");
    assert_eq!(
        WafSolver::default()
            .solve(&challenge)
            .unwrap()
            .split('=')
            .next(),
        Some("waf")
    );
}

#[test]
fn no_challenge_returns_none() {
    assert!(
        WafChallenge::parse_html("<html>normal page</html>", 100)
            .unwrap()
            .is_none()
    );
}

#[test]
fn cookie_name_injection_is_rejected() {
    let encoded = encoded_challenge(b"x", 1);
    assert!(WafChallenge::from_encoded(&encoded, "bad=name", 10).is_err());
}
