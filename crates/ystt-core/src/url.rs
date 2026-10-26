//! TikTok URL parsing and post-ID extraction.
//!
//! The parser deliberately avoids regular expressions: path segments, query
//! pairs and small HTML attribute scans are cheaper and cannot panic on a
//! malformed static pattern.

use url::Url;

use crate::error::{Result, YsttError};
use crate::model::MediaKind;

/// Parsed representation of a TikTok URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TikTokUrl {
    raw: String,
    host: String,
    post_id: Option<String>,
    user: Option<String>,
    short_code: Option<String>,
    kind: Option<MediaKind>,
}

impl TikTokUrl {
    /// Parse a TikTok URL in any of the supported forms.
    pub fn parse(input: &str) -> Result<Self> {
        let raw = input.trim();
        if raw.is_empty() {
            return Err(YsttError::InvalidUrl(input.to_owned()));
        }

        let normalized = normalize_input(raw);
        let url = Url::parse(&normalized).map_err(|_| YsttError::InvalidUrl(input.to_owned()))?;
        let host = url.host_str().unwrap_or_default().to_ascii_lowercase();

        if !(host == "tiktok.com" || host.ends_with(".tiktok.com")) {
            return Err(YsttError::UnsupportedUrl(input.to_owned()));
        }

        let segments: Vec<&str> = url
            .path_segments()
            .into_iter()
            .flatten()
            .filter(|segment| !segment.is_empty())
            .collect();

        let mut user = None;
        let mut kind = None;
        let mut post_id = None;
        let mut short_code = None;

        if let Some(first) = segments.first() {
            if let Some(candidate) = first.strip_prefix('@')
                && segments.len() >= 3
                && is_post_kind(segments[1])
                && is_post_id(segments[2])
            {
                user = Some(candidate.to_owned());
                kind = parse_kind(segments[1]);
                post_id = Some(segments[2].to_owned());
            } else if *first == "t"
                && let Some(code) = segments.get(1)
                && is_short_code(code)
            {
                short_code = Some((*code).to_owned());
            }
        }

        if post_id.is_none() {
            for pair in segments.windows(2) {
                if is_post_kind(pair[0]) && is_post_id(pair[1]) {
                    kind = parse_kind(pair[0]);
                    post_id = Some(pair[1].to_owned());
                    break;
                }
            }
        }

        if post_id.is_none() {
            post_id = url
                .query_pairs()
                .find(|(key, value)| {
                    (key == "item_id" || key == "share_item_id") && is_post_id(value)
                })
                .map(|(_, value)| value.into_owned());
        }

        if post_id.is_none() {
            post_id = segments
                .iter()
                .rev()
                .find(|segment| is_post_id(segment))
                .map(|segment| (*segment).to_owned());
        }

        if (host == "vm.tiktok.com" || host == "vt.tiktok.com") && short_code.is_none() {
            short_code = segments
                .first()
                .filter(|segment| is_short_code(segment))
                .map(|segment| (*segment).to_owned());
        }

        Ok(Self {
            raw: raw.to_owned(),
            host,
            post_id,
            user,
            short_code,
            kind,
        })
    }

    /// Original input.
    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// Lowercase host.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Post ID, when it can be extracted without a network request.
    pub fn post_id(&self) -> Option<&str> {
        self.post_id.as_deref()
    }

    /// Author handle, when present in the URL.
    pub fn user(&self) -> Option<&str> {
        self.user.as_deref()
    }

    /// Short-link code (`vm.`, `vt.` or `/t/`).
    pub fn short_code(&self) -> Option<&str> {
        self.short_code.as_deref()
    }

    /// Post kind, when present in the URL.
    pub fn kind(&self) -> Option<MediaKind> {
        self.kind
    }

    /// Whether this URL needs a short-link redirect resolution.
    pub fn is_short_link(&self) -> bool {
        self.short_code.is_some()
    }

    /// Build the canonical page URL for a post.
    ///
    /// TikTok serves the rehydration payload for both videos and photo posts
    /// on the `/video/<id>` path, so the photo path is intentionally not used.
    pub fn page_url(&self, post_id: &str) -> String {
        match self.user.as_deref() {
            Some(user) => format!("https://www.tiktok.com/@{user}/video/{post_id}"),
            None => format!("https://www.tiktok.com/@i/video/{post_id}"),
        }
    }

    /// Build the short URL to resolve, if any.
    pub fn short_url(&self) -> Option<String> {
        let code = self.short_code.as_deref()?;
        match self.host.as_str() {
            "vm.tiktok.com" => Some(format!("https://vm.tiktok.com/{code}")),
            "vt.tiktok.com" => Some(format!("https://vt.tiktok.com/{code}")),
            _ => Some(format!("https://www.tiktok.com/t/{code}")),
        }
    }
}

/// Extract a post ID from any text that contains a TikTok URL or a bare item ID.
pub fn get_post_id(input: &str) -> Option<String> {
    let normalized = normalize_input(input);
    if let Ok(parsed) = TikTokUrl::parse(&normalized)
        && let Some(post_id) = parsed.post_id()
    {
        return Some(post_id.to_owned());
    }
    scan_post_id(&normalized)
}

/// Find a post ID in a short-link HTML page.
pub fn find_post_id_in_html(html: &str) -> Option<String> {
    if let Some(href) = find_tag_attribute(html, "link", "rel", "canonical", "href")
        .and_then(|href| get_post_id(&href))
    {
        return Some(href);
    }
    if let Some(content) = find_tag_attribute(html, "meta", "property", "og:url", "content")
        .and_then(|content| get_post_id(&content))
    {
        return Some(content);
    }
    if let Some(anchor) = find_tag_name(html, "a")
        .and_then(|tag| extract_attribute(tag, "href"))
        .and_then(get_post_id)
    {
        return Some(anchor);
    }
    find_json_item_id(html)
}

fn normalize_input(input: &str) -> String {
    let mut value = input.trim().replace("\\u002f", "/").replace("\\u002F", "/");
    if !value.contains("://") {
        value = format!("https://{value}");
    }
    value
}

fn parse_kind(value: &str) -> Option<MediaKind> {
    match value.to_ascii_lowercase().as_str() {
        "video" => Some(MediaKind::Video),
        "photo" => Some(MediaKind::Photo),
        _ => None,
    }
}

fn is_post_kind(value: &str) -> bool {
    value.eq_ignore_ascii_case("video") || value.eq_ignore_ascii_case("photo")
}

fn is_post_id(value: &str) -> bool {
    value.len() >= 3 && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn is_short_code(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

fn scan_post_id(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();

    for marker in ["/video/", "/photo/"] {
        let mut search_from = 0;
        while let Some(index) = lower[search_from..].find(marker) {
            let start = search_from + index + marker.len();
            if let Some(id) = digits_at(text, start) {
                return Some(id);
            }
            search_from = start;
        }
    }

    for marker in ["item_id=", "share_item_id="] {
        if let Some(index) = lower.find(marker)
            && let Some(id) = digits_at(text, index + marker.len())
        {
            return Some(id);
        }
    }

    let bytes = text.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'/'
            && let Some(id) = digits_at(text, index + 1)
            && id.len() >= 17
        {
            return Some(id);
        }
    }

    None
}

fn digits_at(text: &str, start: usize) -> Option<String> {
    let bytes = text.as_bytes();
    let mut end = start;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    if end > start {
        Some(text[start..end].to_owned())
    } else {
        None
    }
}

fn find_tag_name<'a>(html: &'a str, name: &str) -> Option<&'a str> {
    let bytes = html.as_bytes();
    let mut cursor = 0;
    while let Some(relative) = html[cursor..].find('<') {
        let start = cursor + relative;
        let mut index = start + 1;
        if index < bytes.len() && bytes[index] == b'/' {
            index += 1;
        }
        let name_start = index;
        while index < bytes.len() && (bytes[index].is_ascii_alphanumeric()) {
            index += 1;
        }
        let tag_name = &html[name_start..index];
        let end_relative = html[start..].find('>')?;
        let end = start + end_relative + 1;
        if tag_name.eq_ignore_ascii_case(name) {
            return Some(&html[start..end]);
        }
        cursor = end;
    }
    None
}

fn find_tag_attribute(
    html: &str,
    tag_name: &str,
    attribute: &str,
    expected: &str,
    output: &str,
) -> Option<String> {
    let mut search_from = 0;
    while let Some(tag) = find_tag_name(&html[search_from..], tag_name) {
        if extract_attribute(tag, attribute)
            .is_some_and(|value| value.eq_ignore_ascii_case(expected))
            && let Some(value) = extract_attribute(tag, output)
        {
            return Some(value.to_owned());
        }
        let consumed = tag.as_ptr() as usize - html.as_ptr() as usize + tag.len();
        search_from = consumed;
        if search_from >= html.len() {
            break;
        }
    }
    None
}

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

fn find_json_item_id(html: &str) -> Option<String> {
    let marker = "\"itemId\"";
    let lower = html.to_ascii_lowercase();
    let index = lower.find(&marker.to_ascii_lowercase())?;
    let after = &html[index + marker.len()..];
    let colon = after.find(':')?;
    let after_colon = &after[colon + 1..];
    let quote = after_colon.find('"')?;
    digits_at(after_colon, quote + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_direct_video_and_photo() {
        let video = TikTokUrl::parse(
            "https://www.tiktok.com/@some.user/video/1234567890123456789?is_from_webapp=1",
        )
        .unwrap();
        assert_eq!(video.post_id(), Some("1234567890123456789"));
        assert_eq!(video.user(), Some("some.user"));
        assert_eq!(video.kind(), Some(MediaKind::Video));
        assert_eq!(
            video.page_url("1234567890123456789"),
            "https://www.tiktok.com/@some.user/video/1234567890123456789"
        );

        let photo =
            TikTokUrl::parse("https://www.tiktok.com/@u/photo/9876543210987654321").unwrap();
        assert_eq!(photo.kind(), Some(MediaKind::Photo));
        assert_eq!(
            photo.page_url("9876543210987654321"),
            "https://www.tiktok.com/@u/video/9876543210987654321"
        );
    }

    #[test]
    fn parses_short_links() {
        let vm = TikTokUrl::parse("https://vm.tiktok.com/ZMabcDEF/").unwrap();
        assert!(vm.is_short_link());
        assert_eq!(vm.short_code(), Some("ZMabcDEF"));
        assert_eq!(
            vm.short_url().as_deref(),
            Some("https://vm.tiktok.com/ZMabcDEF")
        );

        let t = TikTokUrl::parse("https://www.tiktok.com/t/ZTabc123/").unwrap();
        assert_eq!(t.short_code(), Some("ZTabc123"));
        assert_eq!(
            t.short_url().as_deref(),
            Some("https://www.tiktok.com/t/ZTabc123")
        );
    }

    #[test]
    fn parses_query_item_id_and_bare_long_id() {
        let query =
            TikTokUrl::parse("https://www.tiktok.com/@u?item_id=1234567890123456789").unwrap();
        assert_eq!(query.post_id(), Some("1234567890123456789"));

        let bare = TikTokUrl::parse("https://www.tiktok.com/1234567890123456789").unwrap();
        assert_eq!(bare.post_id(), Some("1234567890123456789"));
    }

    #[test]
    fn rejects_non_tiktok_and_garbage() {
        assert!(matches!(
            TikTokUrl::parse("https://example.com/video/1"),
            Err(YsttError::UnsupportedUrl(_))
        ));
        assert!(matches!(
            TikTokUrl::parse("not a url"),
            Err(YsttError::InvalidUrl(_))
        ));
    }

    #[test]
    fn accepts_scheme_less_input() {
        let url = TikTokUrl::parse("www.tiktok.com/@u/video/123").unwrap();
        assert_eq!(url.post_id(), Some("123"));
    }

    #[test]
    fn extracts_post_id_from_html() {
        let html = r#"
            <link rel="canonical" href="https://www.tiktok.com/@u/video/1234567890123456789">
        "#;
        assert_eq!(
            find_post_id_in_html(html).as_deref(),
            Some("1234567890123456789")
        );

        let json = r#"{"itemId":"9876543210987654321"}"#;
        assert_eq!(
            find_post_id_in_html(json).as_deref(),
            Some("9876543210987654321")
        );
    }

    #[test]
    fn extracts_post_id_from_arbitrary_text() {
        assert_eq!(get_post_id("https://vm.tiktok.com/ZMabc/").as_deref(), None);
        assert_eq!(
            get_post_id("https://www.tiktok.com/@u/photo/123").as_deref(),
            Some("123")
        );
    }
}
