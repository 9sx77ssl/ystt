//! Parsing and representation of TikTok's WAF proof-of-work challenge.
//!
//! TikTok serves a small HTML page containing two `<p>` tags:
//!
//! ```html
//! <p id="cs" class="<base64 challenge>"></p>
//! <p id="wci" class="<cookie name>"></p>
//! ```
//!
//! The base64 challenge decodes to JSON with at least:
//!
//! ```json
//! { "v": { "a": "<base64 prefix>", "c": "<base64 sha256 digest>" }, "d": "" }
//! ```
//!
//! The client must find an integer `n >= 0` such that
//! `SHA256(prefix || decimal(n)) == digest`, put `base64(decimal(n))` into `d`,
//! and send the whole JSON, base64-encoded, as the cookie named by `wci`.

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// Errors produced by the WAF module.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum WafError {
    /// The page does not contain a WAF challenge.
    #[error("WAF challenge not found")]
    NotFound,

    /// The challenge is malformed.
    #[error("invalid WAF challenge: {0}")]
    Invalid(String),

    /// No nonce was found within the configured iteration budget.
    #[error("WAF challenge not solved after {0} candidates")]
    NotSolved(u64),

    /// The challenge cookie name is unsafe or empty.
    #[error("invalid WAF cookie name: {0}")]
    InvalidCookieName(String),
}

/// A parsed WAF challenge, ready to be solved.
#[derive(Debug, Clone)]
pub struct WafChallenge {
    /// Cookie name from the `wci` tag (for example `waf_cookie`).
    pub cookie_name: String,
    /// Original challenge JSON. Extra fields are preserved verbatim.
    pub payload: Value,
    /// Decoded `v.a` bytes, the SHA-256 prefix.
    pub prefix: Vec<u8>,
    /// Decoded `v.c`, the expected 32-byte SHA-256 digest.
    pub expected: [u8; 32],
    /// Inclusive upper bound for the nonce search.
    pub max_iterations: u64,
}

impl WafChallenge {
    /// Parse a challenge from an HTML page.
    ///
    /// Returns `Ok(None)` when the page has no `cs` / `wci` tags. Returns an
    /// error when the tags exist but the payload cannot be decoded.
    pub fn parse_html(html: &str, max_iterations: u64) -> Result<Option<Self>, WafError> {
        let Some(challenge_tag) = find_tag_with_id(html, "cs") else {
            return Ok(None);
        };
        let Some(cookie_tag) = find_tag_with_id(html, "wci") else {
            return Ok(None);
        };

        let Some(encoded) = extract_attribute(challenge_tag, "class") else {
            return Err(WafError::Invalid("cs tag has no class attribute".into()));
        };
        let Some(cookie_name) = extract_attribute(cookie_tag, "class") else {
            return Err(WafError::Invalid("wci tag has no class attribute".into()));
        };

        Self::from_encoded(encoded, cookie_name, max_iterations).map(Some)
    }

    /// Build a challenge from the encoded `cs` payload and the cookie name.
    pub fn from_encoded(
        encoded: &str,
        cookie_name: &str,
        max_iterations: u64,
    ) -> Result<Self, WafError> {
        let cookie_name = cookie_name.trim();
        validate_cookie_name(cookie_name)?;

        let decoded = decode_base64(encoded.trim())
            .ok_or_else(|| WafError::Invalid("cs is not valid base64".into()))?;
        let payload: Value = serde_json::from_slice(&decoded)
            .map_err(|error| WafError::Invalid(format!("cs is not valid JSON: {error}")))?;

        let version = payload
            .get("v")
            .and_then(Value::as_object)
            .ok_or_else(|| WafError::Invalid("challenge has no v object".into()))?;

        let prefix = decode_field(version, "a")?;
        let expected_bytes = decode_digest(version.get("c"))?;
        if prefix.is_empty() {
            return Err(WafError::Invalid("challenge prefix is empty".into()));
        }
        let expected: [u8; 32] = expected_bytes.as_slice().try_into().map_err(|_| {
            WafError::Invalid(format!(
                "challenge digest is {} bytes, expected 32",
                expected_bytes.len()
            ))
        })?;

        Ok(Self {
            cookie_name: cookie_name.to_owned(),
            payload,
            prefix,
            expected,
            max_iterations,
        })
    }

    /// Build a cookie value for a known nonce.
    ///
    /// The nonce is inserted as `d = base64(decimal(nonce))`; all other
    /// challenge fields, including unknown ones, are preserved.
    pub fn cookie_for(&self, nonce: u64) -> Result<String, WafError> {
        let mut payload: Map<String, Value> = match self.payload.clone() {
            Value::Object(map) => map,
            _ => {
                return Err(WafError::Invalid(
                    "challenge payload is not an object".into(),
                ));
            }
        };

        let nonce = nonce.to_string();
        payload.insert("d".into(), Value::String(STANDARD.encode(nonce)));

        let json = serde_json::to_vec(&Value::Object(payload))
            .map_err(|error| WafError::Invalid(format!("cannot serialize challenge: {error}")))?;
        let value = STANDARD.encode(json);
        Ok(format!("{}={}", self.cookie_name, value))
    }

    /// Stable fingerprint of the challenge, used for the solved-cookie cache.
    pub fn fingerprint(&self) -> String {
        let mut hash = Sha256::new();
        hash.update(self.cookie_name.as_bytes());
        hash.update([0]);
        hash.update(&self.prefix);
        hash.update(self.expected);
        hex_encode(&hash.finalize())
    }

    /// Returns the expected number of candidates before a random solution.
    ///
    /// For a uniform nonce in `[0, max_iterations]`, the expectation is
    /// `max_iterations / 2`.
    pub fn expected_iterations(&self) -> f64 {
        self.max_iterations as f64 / 2.0
    }
}

fn decode_field(object: &Map<String, Value>, field: &str) -> Result<Vec<u8>, WafError> {
    let encoded = object
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| WafError::Invalid(format!("challenge has no v.{field} string")))?;
    decode_base64(encoded)
        .ok_or_else(|| WafError::Invalid(format!("v.{field} is not valid base64")))
}

fn decode_digest(value: Option<&Value>) -> Result<Vec<u8>, WafError> {
    let encoded = value
        .and_then(Value::as_str)
        .ok_or_else(|| WafError::Invalid("challenge has no v.c string".into()))?;
    if let Some(bytes) = decode_base64(encoded)
        && bytes.len() == 32
    {
        return Ok(bytes);
    }
    if encoded.len() == 64 && encoded.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        let bytes =
            decode_hex(encoded).ok_or_else(|| WafError::Invalid("v.c is invalid hex".into()))?;
        if bytes.len() == 32 {
            return Ok(bytes);
        }
    }
    Err(WafError::Invalid(
        "v.c is neither a 32-byte base64 nor a 64-character hex digest".into(),
    ))
}

fn decode_base64(value: &str) -> Option<Vec<u8>> {
    let value = value.trim().trim_end_matches('=');
    STANDARD
        .decode(value)
        .or_else(|_| STANDARD_NO_PAD.decode(value))
        .or_else(|_| URL_SAFE.decode(value))
        .or_else(|_| URL_SAFE_NO_PAD.decode(value))
        .ok()
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return None;
    }
    let mut result = Vec::with_capacity(value.len() / 2);
    let bytes = value.as_bytes();
    for pair in bytes.as_chunks::<2>().0 {
        let high = hex_nibble(pair[0])?;
        let low = hex_nibble(pair[1])?;
        result.push(high << 4 | low);
    }
    Some(result)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn validate_cookie_name(name: &str) -> Result<(), WafError> {
    if name.is_empty()
        || name.contains(';')
        || name.contains('=')
        || name.contains(',')
        || name.chars().any(char::is_control)
    {
        return Err(WafError::InvalidCookieName(name.to_owned()));
    }
    Ok(())
}

/// Find a `<p ... id="..." ...>` tag and return the whole tag slice.
///
/// This intentionally does not use an HTML parser: the WAF page is tiny and
/// the tag shape is stable. It tolerates attribute order, extra attributes,
/// single or double quotes, and uppercase tag/attribute names.
fn find_tag_with_id<'a>(html: &'a str, id: &str) -> Option<&'a str> {
    let bytes = html.as_bytes();
    let mut cursor = 0usize;

    while let Some(relative) = html[cursor..].find('<') {
        let start = cursor + relative;
        if !tag_name_is_p(bytes, start + 1) {
            cursor = start + 1;
            continue;
        }

        let end_relative = html[start..].find('>')?;
        let end = start + end_relative + 1;
        let tag = &html[start..end];
        if tag_has_id(tag, id) {
            return Some(tag);
        }
        cursor = end;
    }

    None
}

fn tag_name_is_p(bytes: &[u8], mut index: usize) -> bool {
    if index >= bytes.len() || !bytes[index].eq_ignore_ascii_case(&b'p') {
        return false;
    }
    index += 1;
    matches!(
        bytes.get(index),
        Some(byte) if byte.is_ascii_whitespace() || *byte == b'>' || *byte == b'/'
    )
}

fn tag_has_id(tag: &str, id: &str) -> bool {
    let Some(value) = extract_attribute(tag, "id") else {
        return false;
    };
    value.eq_ignore_ascii_case(id)
}

/// Extract an attribute value from a single tag.
///
/// The returned slice excludes the quotes. Attribute names are matched
/// case-insensitively; values are returned verbatim.
fn extract_attribute<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let lower = tag.to_ascii_lowercase();
    let needle = format!("{name}=");
    let mut search_from = 0usize;

    while let Some(relative) = lower[search_from..].find(&needle) {
        let index = search_from + relative;
        let before_is_boundary = index == 0
            || lower.as_bytes()[index - 1].is_ascii_whitespace()
            || lower.as_bytes()[index - 1] == b'<';
        if !before_is_boundary {
            search_from = index + needle.len();
            continue;
        }

        let mut value_start = index + needle.len();
        while lower
            .as_bytes()
            .get(value_start)
            .is_some_and(u8::is_ascii_whitespace)
        {
            value_start += 1;
        }

        let quote = *lower.as_bytes().get(value_start)?;
        if quote != b'"' && quote != b'\'' {
            return None;
        }
        value_start += 1;
        let value_end = lower[value_start..].find(quote as char)? + value_start;
        return tag.get(value_start..value_end);
    }

    None
}

/// Convenience: compute one SHA-256 candidate digest.
///
/// Kept public for tests and for external verification of a solve.
pub fn candidate_digest(prefix: &[u8], nonce: u64) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(prefix);
    hash.update(nonce.to_string().as_bytes());
    hash.finalize().into()
}

/// Heuristic detection of a non-WAF bot page.
///
/// ystt solves TikTok's own proof-of-work challenge. If a response is a
/// Cloudflare/other CAPTCHA page, there is no honest automatic solution; the
/// caller should report a typed parse error instead of searching forever.
pub fn looks_like_external_bot_page(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    [
        "cf-chl",
        "challenge-platform",
        "captcha",
        "verify you are human",
        "are you a robot",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn known_challenge(prefix: &[u8], nonce: u64) -> (String, String) {
        let expected = candidate_digest(prefix, nonce);
        let payload = json!({
            "v": {
                "a": STANDARD.encode(prefix),
                "c": STANDARD.encode(expected),
            },
            "d": "",
            "extra": {"keep": true},
        });
        let encoded = STANDARD.encode(serde_json::to_vec(&payload).unwrap());
        (encoded, "waf_cookie".to_owned())
    }

    #[test]
    fn parses_challenge_tags_in_any_order() {
        let prefix = b"prefix";
        let (encoded, cookie) = known_challenge(prefix, 99);
        let html = format!(
            r#"<html><p class="ignored"></p><p data-x="1" id='cs' class="{encoded}"></p><P id='wci' class='{cookie}'></P></html>"#
        );

        let challenge = WafChallenge::parse_html(&html, 1_000).unwrap().unwrap();
        assert_eq!(challenge.cookie_name, "waf_cookie");
        assert_eq!(challenge.prefix, prefix);
        assert_eq!(challenge.expected, candidate_digest(prefix, 99));
    }

    #[test]
    fn no_challenge_returns_none() {
        assert!(
            WafChallenge::parse_html("<html>hello</html>", 10)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn cookie_preserves_extra_fields() {
        let (encoded, cookie) = known_challenge(b"abc", 7);
        let challenge = WafChallenge::from_encoded(&encoded, &cookie, 10).unwrap();
        let value = challenge.cookie_for(7).unwrap();
        assert!(value.starts_with("waf_cookie="));

        let raw = STANDARD
            .decode(value.trim_start_matches("waf_cookie="))
            .unwrap();
        let payload: Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(payload["d"], STANDARD.encode("7"));
        assert_eq!(payload["extra"]["keep"], true);
    }

    #[test]
    fn rejects_bad_cookie_name() {
        let (encoded, _) = known_challenge(b"abc", 1);
        assert!(WafChallenge::from_encoded(&encoded, "bad=name", 10).is_err());
    }

    #[test]
    fn accepts_url_safe_base64_and_hex_digest() {
        let prefix = b"url-safe";
        let nonce = 1234u64;
        let expected = candidate_digest(prefix, nonce);
        let payload = json!({
            "v": {
                "a": URL_SAFE.encode(prefix),
                "c": hex_encode(&expected),
            },
            "d": "",
        });
        let encoded = URL_SAFE.encode(serde_json::to_vec(&payload).unwrap());
        let challenge = WafChallenge::from_encoded(&encoded, "waf", 10_000).unwrap();
        assert_eq!(challenge.expected, expected);
    }

    #[test]
    fn fingerprint_is_stable_and_distinct() {
        let (encoded, cookie) = known_challenge(b"fingerprint", 42);
        let first = WafChallenge::from_encoded(&encoded, &cookie, 100).unwrap();
        let second = WafChallenge::from_encoded(&encoded, &cookie, 100).unwrap();
        assert_eq!(first.fingerprint(), second.fingerprint());

        let (other, _) = known_challenge(b"other", 42);
        let other = WafChallenge::from_encoded(&other, "waf", 100).unwrap();
        assert_ne!(first.fingerprint(), other.fingerprint());
    }

    #[test]
    fn detects_external_bot_pages() {
        assert!(looks_like_external_bot_page(
            "<html><div class=\"cf-chl\"></div></html>"
        ));
        assert!(looks_like_external_bot_page("Please verify you are human"));
        assert!(!looks_like_external_bot_page("<html>normal</html>"));
    }
}
