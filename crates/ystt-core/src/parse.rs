//! Parsing of TikTok's `__UNIVERSAL_DATA_FOR_REHYDRATION__` payload.

use memchr::memmem;

use crate::error::{Result, YsttError};
use crate::model::{ItemStruct, TikTokItem, UniversalData};

/// Marker ID that carries the server-rendered post JSON.
const MARKER: &[u8] = b"__UNIVERSAL_DATA_FOR_REHYDRATION__";

/// Result of trying to parse a TikTok page.
#[derive(Debug, Clone)]
pub enum PageParse {
    /// A usable post was found.
    Item(Box<TikTokItem>),
    /// The page is a shell/challenge with no item payload.
    NoItem,
}

/// Extract the JSON string from the rehydration script tag.
///
/// The function tolerates attribute order and whitespace around the tag.
pub fn extract_universal_json(html: &str) -> Option<&str> {
    let bytes = html.as_bytes();
    let marker = memmem::find(bytes, MARKER)?;
    let after_marker = &bytes[marker..];
    let tag_end = memchr::memchr(b'>', after_marker)?;
    let json_start = marker + tag_end + 1;
    let rest = &html[json_start..];
    let json_end = memmem::find(rest.as_bytes(), b"</script>")?;
    Some(rest[..json_end].trim())
}

/// Parse a TikTok page for a specific post ID.
pub fn parse_page(html: &str, expected_post_id: &str) -> Result<PageParse> {
    let Some(json) = extract_universal_json(html) else {
        return Ok(PageParse::NoItem);
    };

    let data: UniversalData = serde_json::from_str(json)
        .map_err(|error| YsttError::Parse(format!("invalid universal data JSON: {error}")))?;

    let Some(video_detail) = data.default_scope.and_then(|scope| scope.video_detail) else {
        return Ok(PageParse::NoItem);
    };

    let Some(item_struct) = video_detail
        .item_info
        .and_then(|item_info| item_info.item_struct)
    else {
        if let Some(status) = video_detail
            .status_msg
            .filter(|status| !status.is_empty() && !status.eq_ignore_ascii_case("success"))
        {
            return Err(YsttError::Unavailable(status));
        }
        return Ok(PageParse::NoItem);
    };

    let item: ItemStruct = item_struct;
    let item = item.into_item(expected_post_id)?;
    Ok(PageParse::Item(Box::new(item)))
}

/// Parse TikTok's official embed page (`/embed/v2/<id>`).
///
/// The embed page is smaller than the full page and returns a direct video URL
/// without the `tt_chain_token` cookie requirement. It does not expose gallery
/// images, so callers must fall back to the full page for photo posts.
pub fn parse_embed_page(html: &str, expected_post_id: &str) -> Option<TikTokItem> {
    let json = extract_script_json(html, "__FRONTITY_CONNECT_STATE__")?;
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let data = value.get("source")?.get("data")?.as_object()?;

    for entry in data.values() {
        let Some(video_data) = entry.get("videoData") else {
            continue;
        };
        let Some(item) = video_data.get("itemInfos") else {
            continue;
        };
        let id = item.get("id").and_then(serde_json::Value::as_str)?;
        if id != expected_post_id {
            continue;
        }
        let urls = item
            .get("video")?
            .get("urls")?
            .as_array()?
            .iter()
            .filter_map(serde_json::Value::as_str)
            .filter(|url| crate::model::is_https_url(url))
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if urls.is_empty() {
            return None;
        }
        let meta = item.get("video").and_then(|video| video.get("videoMeta"));
        let width = meta
            .and_then(|meta| meta.get("width"))
            .and_then(serde_json::Value::as_u64)
            .map(|value| value as u32);
        let height = meta
            .and_then(|meta| meta.get("height"))
            .and_then(serde_json::Value::as_u64)
            .map(|value| value as u32);
        let duration = meta
            .and_then(|meta| meta.get("duration"))
            .and_then(serde_json::Value::as_u64)
            .map(|value| value as u32);
        let author = item
            .get("authorId")
            .and_then(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .or_else(|| value.as_u64().map(|id| id.to_string()))
            })
            .unwrap_or_else(|| "unknown".into());
        let description = item
            .get("text")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);

        return Some(TikTokItem {
            post_id: id.to_owned(),
            author: crate::model::Author {
                id: author,
                nickname: None,
            },
            description,
            video: Some(crate::model::VideoInfo {
                play_url: urls.first().cloned(),
                download_url: None,
                bitrates: urls
                    .iter()
                    .map(|url| crate::model::Bitrate {
                        bitrate: 0,
                        codec: None,
                        url: Some(url.clone()),
                        width,
                        height,
                    })
                    .collect(),
                width,
                height,
                duration_sec: duration,
                cover_url: None,
            }),
            photos: Vec::new(),
            music: None,
            is_content_classified: false,
        });
    }

    None
}

fn extract_script_json<'a>(html: &'a str, id: &str) -> Option<&'a str> {
    let marker = format!("id=\"{id}\"");
    let marker_index = html.find(&marker)?;
    let tag_start = html[..marker_index].rfind("<script")?;
    let tag_end = html[tag_start..].find('>')? + tag_start;
    let content_start = tag_end + 1;
    let content_end = html[content_start..].find("</script>")? + content_start;
    Some(html[content_start..content_end].trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page_with(item_struct: serde_json::Value) -> String {
        let data = serde_json::json!({
            "__DEFAULT_SCOPE__": {
                "webapp.video-detail": {
                    "statusMsg": "",
                    "itemInfo": { "itemStruct": item_struct }
                }
            }
        });
        format!(
            r#"<html><body><script id="__UNIVERSAL_DATA_FOR_REHYDRATION__" type="application/json">{data}</script></body></html>"#
        )
    }

    #[test]
    fn parses_video_item() {
        let html = page_with(serde_json::json!({
            "id": "123",
            "desc": "hello",
            "author": { "uniqueId": "tester", "nickname": "Tester" },
            "video": {
                "playAddr": "https://cdn.example/video.mp4",
                "bitrateInfo": [
                    { "Bitrate": 1000, "CodecType": "h264", "PlayAddr": { "UrlList": ["https://cdn.example/1000.mp4"] } },
                    { "Bitrate": 2000, "CodecType": "bytevc1", "PlayAddr": { "UrlList": ["https://cdn.example/2000.mp4"] } }
                ]
            },
            "music": { "playUrl": "https://cdn.example/audio.mp3", "title": "song" },
            "isContentClassified": false
        }));

        let PageParse::Item(item) = parse_page(&html, "123").unwrap() else {
            panic!("expected item");
        };
        assert_eq!(item.post_id, "123");
        assert_eq!(item.author.id, "tester");
        assert_eq!(item.description.as_deref(), Some("hello"));
        assert_eq!(
            item.best_video_url(crate::config::QualityPolicy::Best),
            Some("https://cdn.example/2000.mp4")
        );
        assert_eq!(item.audio_url(), Some("https://cdn.example/audio.mp3"));
    }

    #[test]
    fn parses_slideshow_and_keeps_photo_order() {
        let html = page_with(serde_json::json!({
            "id": "456",
            "author": { "uniqueId": "slides" },
            "imagePost": {
                "images": [
                    { "imageURL": { "urlList": ["https://cdn/p1.webp", "https://cdn/p1.jpeg?x=1"] } },
                    { "imageURL": { "urlList": ["https://cdn/p2.jpeg?x=1"] } }
                ]
            },
            "music": { "playUrl": "https://cdn/music.mp3" }
        }));

        let PageParse::Item(item) = parse_page(&html, "456").unwrap() else {
            panic!("expected item");
        };
        assert_eq!(item.photos.len(), 2);
        assert_eq!(
            item.photo_urls(),
            vec!["https://cdn/p1.jpeg?x=1", "https://cdn/p2.jpeg?x=1"]
        );
    }

    #[test]
    fn missing_marker_is_no_item() {
        assert!(matches!(
            parse_page("<html>no data</html>", "1").unwrap(),
            PageParse::NoItem
        ));
    }

    #[test]
    fn empty_payload_is_no_item() {
        let html = r#"<script id="__UNIVERSAL_DATA_FOR_REHYDRATION__" type="application/json">{"__DEFAULT_SCOPE__":{"webapp.video-detail":{"statusMsg":""}}}</script>"#;
        assert!(matches!(parse_page(html, "1").unwrap(), PageParse::NoItem));
    }

    #[test]
    fn unavailable_status_is_an_error() {
        let html = r#"<script id="__UNIVERSAL_DATA_FOR_REHYDRATION__" type="application/json">{"__DEFAULT_SCOPE__":{"webapp.video-detail":{"statusMsg":"This post is unavailable"}}}</script>"#;
        assert!(matches!(
            parse_page(html, "1"),
            Err(YsttError::Unavailable(_))
        ));
    }

    #[test]
    fn malformed_json_is_a_parse_error() {
        let html = r#"<script id="__UNIVERSAL_DATA_FOR_REHYDRATION__" type="application/json">{not json}</script>"#;
        assert!(matches!(parse_page(html, "1"), Err(YsttError::Parse(_))));
    }

    #[test]
    fn extracts_json_with_attr_order_tolerance() {
        let html = r#"<script type="application/json" id="__UNIVERSAL_DATA_FOR_REHYDRATION__">{"a":1}</script>"#;
        assert_eq!(extract_universal_json(html), Some(r#"{"a":1}"#));
    }

    #[test]
    fn parses_embed_video_without_cookies() {
        let html = r#"
            <script id="__FRONTITY_CONNECT_STATE__" type="application/json">
              {"source":{"data":{"/embed/v2/123":{"videoData":{"itemInfos":{
                "id":"123","text":"embed","authorId":"author",
                "video":{"urls":["https://cdn.example/e.mp4"],"videoMeta":{"width":720,"height":1280,"duration":10}}
              }}}}}}
            </script>
        "#;
        let item = parse_embed_page(html, "123").unwrap();
        assert_eq!(item.post_id, "123");
        assert_eq!(item.author.id, "author");
        assert_eq!(item.video.as_ref().unwrap().width, Some(720));
        assert_eq!(
            item.best_video_url(crate::config::QualityPolicy::Balanced),
            Some("https://cdn.example/e.mp4")
        );
    }

    #[test]
    fn embed_without_video_is_not_usable_for_galleries() {
        let html = r#"
            <script id="__FRONTITY_CONNECT_STATE__" type="application/json">
              {"source":{"data":{"/embed/v2/456":{"videoData":{"itemInfos":{
                "id":"456","video":{"urls":[],"videoMeta":{}}
              }}}}}}
            </script>
        "#;
        assert!(parse_embed_page(html, "456").is_none());
    }
}
