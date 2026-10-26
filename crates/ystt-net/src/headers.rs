//! Browser-shaped request headers.
//!
//! `wreq` emulation profiles already provide the TLS, HTTP/2 and default
//! header set. This module adds only the per-request headers that a real
//! browser would send for the current navigation/media context.

use wreq::header::{
    ACCEPT, ACCEPT_ENCODING, COOKIE, HeaderMap, HeaderName, HeaderValue, RANGE, REFERER,
};

use crate::NetError;

/// The kind of request being prepared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    /// A TikTok HTML page navigation.
    Page,
    /// A TikTok short-link resolution request.
    ShortLink,
    /// A video CDN request.
    Video,
    /// A photo CDN request.
    Image,
    /// An audio/music CDN request.
    Audio,
}

const SEC_FETCH_DEST: HeaderName = HeaderName::from_static("sec-fetch-dest");
const SEC_FETCH_MODE: HeaderName = HeaderName::from_static("sec-fetch-mode");
const SEC_FETCH_SITE: HeaderName = HeaderName::from_static("sec-fetch-site");
const SEC_FETCH_USER: HeaderName = HeaderName::from_static("sec-fetch-user");
const UPGRADE_INSECURE_REQUESTS: HeaderName = HeaderName::from_static("upgrade-insecure-requests");

/// Builder for browser-like headers.
#[derive(Debug, Default)]
pub struct BrowserHeaders;

impl BrowserHeaders {
    /// Build headers for one request.
    ///
    /// `referer` should be the canonical TikTok page URL for media requests.
    /// `cookie` is used for the WAF challenge cookie; account cookies are not
    /// part of ystt's design.
    pub fn build(
        kind: RequestKind,
        referer: Option<&str>,
        cookie: Option<&str>,
        range: Option<(u64, u64)>,
    ) -> Result<HeaderMap, NetError> {
        let mut headers = HeaderMap::new();

        match kind {
            RequestKind::Page | RequestKind::ShortLink => {
                insert(
                    &mut headers,
                    ACCEPT,
                    "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8",
                )?;
                insert(&mut headers, SEC_FETCH_DEST, "document")?;
                insert(&mut headers, SEC_FETCH_MODE, "navigate")?;
                insert(&mut headers, SEC_FETCH_SITE, "none")?;
                insert(&mut headers, SEC_FETCH_USER, "?1")?;
                insert(&mut headers, UPGRADE_INSECURE_REQUESTS, "1")?;
            }
            RequestKind::Video => {
                insert(&mut headers, ACCEPT, "*/*")?;
                insert(&mut headers, ACCEPT_ENCODING, "identity")?;
                insert(&mut headers, SEC_FETCH_DEST, "video")?;
                insert(&mut headers, SEC_FETCH_MODE, "no-cors")?;
                insert(&mut headers, SEC_FETCH_SITE, "cross-site")?;
            }
            RequestKind::Image => {
                insert(
                    &mut headers,
                    ACCEPT,
                    "image/avif,image/webp,image/apng,image/svg+xml,image/*,*/*;q=0.8",
                )?;
                insert(&mut headers, ACCEPT_ENCODING, "identity")?;
                insert(&mut headers, SEC_FETCH_DEST, "image")?;
                insert(&mut headers, SEC_FETCH_MODE, "no-cors")?;
                insert(&mut headers, SEC_FETCH_SITE, "cross-site")?;
            }
            RequestKind::Audio => {
                insert(&mut headers, ACCEPT, "audio/*,*/*;q=0.8")?;
                insert(&mut headers, ACCEPT_ENCODING, "identity")?;
                insert(&mut headers, SEC_FETCH_DEST, "audio")?;
                insert(&mut headers, SEC_FETCH_MODE, "no-cors")?;
                insert(&mut headers, SEC_FETCH_SITE, "cross-site")?;
            }
        }

        if let Some(referer) = referer {
            insert(&mut headers, REFERER, referer)?;
        }
        if let Some(cookie) = cookie {
            insert(&mut headers, COOKIE, cookie)?;
        }
        if let Some((start, end)) = range {
            insert(&mut headers, RANGE, &format!("bytes={start}-{end}"))?;
        }

        Ok(headers)
    }
}

fn insert(headers: &mut HeaderMap, name: HeaderName, value: &str) -> Result<(), NetError> {
    let value =
        HeaderValue::from_str(value).map_err(|error| NetError::Header(error.to_string()))?;
    headers.insert(name, value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_headers_are_navigation_shaped() {
        let headers = BrowserHeaders::build(RequestKind::Page, None, None, None).unwrap();
        assert_eq!(headers.get(SEC_FETCH_DEST).unwrap(), "document");
        assert_eq!(headers.get(SEC_FETCH_MODE).unwrap(), "navigate");
        assert_eq!(headers.get(SEC_FETCH_USER).unwrap(), "?1");
    }

    #[test]
    fn video_headers_include_range_referer_and_cookie() {
        let headers = BrowserHeaders::build(
            RequestKind::Video,
            Some("https://www.tiktok.com/@u/video/1"),
            Some("waf_cookie=abc"),
            Some((0, 1023)),
        )
        .unwrap();

        assert_eq!(headers.get(ACCEPT).unwrap(), "*/*");
        assert_eq!(headers.get(RANGE).unwrap(), "bytes=0-1023");
        assert_eq!(headers.get(ACCEPT_ENCODING).unwrap(), "identity");
        assert_eq!(
            headers.get(REFERER).unwrap(),
            "https://www.tiktok.com/@u/video/1"
        );
        assert_eq!(headers.get(COOKIE).unwrap(), "waf_cookie=abc");
        assert_eq!(headers.get(SEC_FETCH_DEST).unwrap(), "video");
    }

    #[test]
    fn image_headers_are_image_shaped() {
        let headers = BrowserHeaders::build(RequestKind::Image, None, None, None).unwrap();
        assert_eq!(headers.get(SEC_FETCH_DEST).unwrap(), "image");
    }
}
