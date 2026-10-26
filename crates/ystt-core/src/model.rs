//! Public media model and private TikTok JSON deserialization types.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::config::QualityPolicy;
use crate::error::Result;
/// Kind of media file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    /// MP4/WebM video.
    Video,
    /// JPEG/PNG/WebP image.
    Photo,
    /// Music/audio track.
    Audio,
}

impl MediaKind {
    /// Stable lowercase name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Video => "video",
            Self::Photo => "photo",
            Self::Audio => "audio",
        }
    }
}

/// Post author.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Author {
    /// TikTok unique ID (handle).
    pub id: String,
    /// Display name when present.
    pub nickname: Option<String>,
}

/// Music metadata attached to a post.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Music {
    /// Playable audio URL.
    pub url: Option<String>,
    /// Track title.
    pub title: Option<String>,
    /// Track author.
    pub author: Option<String>,
}

/// One selectable video stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bitrate {
    /// Declared bitrate in bits per second.
    pub bitrate: u64,
    /// Codec name (`h264`, `bytevc1`, ...).
    pub codec: Option<String>,
    /// Stream URL.
    pub url: Option<String>,
    /// Stream width, when known.
    pub width: Option<u32>,
    /// Stream height, when known.
    pub height: Option<u32>,
}

/// Video streams for a post.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoInfo {
    /// Default playable URL.
    pub play_url: Option<String>,
    /// Download URL, sometimes watermarked.
    pub download_url: Option<String>,
    /// Alternative bitrates.
    pub bitrates: Vec<Bitrate>,
    /// Default stream width, when known.
    pub width: Option<u32>,
    /// Default stream height, when known.
    pub height: Option<u32>,
    /// Duration in seconds, when known.
    pub duration_sec: Option<u32>,
}

/// Image dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageDimensions {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

/// One gallery image and its candidate URLs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageInfo {
    /// Candidate URLs, usually several sizes of the same image.
    pub urls: Vec<String>,
    /// Image width, when known.
    pub width: Option<u32>,
    /// Image height, when known.
    pub height: Option<u32>,
}

/// Fully parsed TikTok post.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TikTokItem {
    /// Post ID.
    pub post_id: String,
    /// Author.
    pub author: Author,
    /// Caption/description.
    pub description: Option<String>,
    /// Video streams, if any.
    pub video: Option<VideoInfo>,
    /// Gallery photos, if any.
    pub photos: Vec<ImageInfo>,
    /// Music metadata.
    pub music: Option<Music>,
    /// TikTok marks some posts as sensitive/age-restricted.
    pub is_content_classified: bool,
}

impl TikTokItem {
    /// Select a video URL according to the quality policy.
    pub fn best_video_url(&self, policy: QualityPolicy) -> Option<&str> {
        let Some(video) = &self.video else {
            return None;
        };

        let bitrates: Vec<&Bitrate> = video
            .bitrates
            .iter()
            .filter(|bitrate| bitrate.url.as_deref().is_some_and(is_https_url))
            .collect();

        if !bitrates.is_empty() {
            let selected = match policy {
                QualityPolicy::Best => bitrates
                    .iter()
                    .copied()
                    .max_by_key(|bitrate| bitrate.bitrate),
                QualityPolicy::Fast => {
                    let usable: Vec<&Bitrate> = bitrates
                        .iter()
                        .copied()
                        .filter(|bitrate| bitrate.height.is_none_or(|height| height >= 360))
                        .collect();
                    let pool = if usable.is_empty() {
                        &bitrates
                    } else {
                        &usable
                    };
                    pool.iter().copied().min_by_key(|bitrate| bitrate.bitrate)
                }
                QualityPolicy::Compatible => {
                    let compatible: Vec<&Bitrate> = bitrates
                        .iter()
                        .copied()
                        .filter(|bitrate| is_h264(bitrate.codec.as_deref()))
                        .collect();
                    let pool = if compatible.is_empty() {
                        bitrates.clone()
                    } else {
                        compatible
                    };
                    pool.iter().copied().max_by_key(|bitrate| bitrate.bitrate)
                }
                QualityPolicy::Balanced => {
                    let within_hd: Vec<&Bitrate> = bitrates
                        .iter()
                        .copied()
                        .filter(|bitrate| {
                            bitrate.width.is_none_or(|width| width <= 1920)
                                && bitrate.height.is_none_or(|height| height <= 1920)
                        })
                        .collect();
                    let pool = if within_hd.is_empty() {
                        bitrates.clone()
                    } else {
                        within_hd
                    };
                    let compatible: Vec<&Bitrate> = pool
                        .iter()
                        .copied()
                        .filter(|bitrate| is_h264(bitrate.codec.as_deref()))
                        .collect();
                    let candidates = if compatible.is_empty() {
                        pool
                    } else {
                        compatible
                    };
                    candidates
                        .iter()
                        .copied()
                        .max_by_key(|bitrate| bitrate.bitrate)
                }
            };

            if let Some(bitrate) = selected
                && let Some(url) = &bitrate.url
            {
                return Some(url);
            }
        }

        video
            .play_url
            .as_deref()
            .filter(|url| is_https_url(url))
            .or_else(|| {
                video
                    .download_url
                    .as_deref()
                    .filter(|url| is_https_url(url))
            })
    }

    /// Pick one URL per gallery image, preferring JPEG.
    pub fn photo_urls(&self) -> Vec<String> {
        self.photo_entries()
            .into_iter()
            .map(|(url, _)| url)
            .collect()
    }

    /// Pick gallery URLs and their dimensions, preserving order and deduplicating.
    pub fn photo_entries(&self) -> Vec<(String, Option<ImageDimensions>)> {
        let mut seen = HashSet::new();
        let mut result = Vec::with_capacity(self.photos.len());

        for photo in &self.photos {
            let Some(url) = pick_image_url(&photo.urls).filter(|url| is_https_url(url)) else {
                continue;
            };
            if !seen.insert(url.clone()) {
                continue;
            }
            let dimensions = photo
                .width
                .zip(photo.height)
                .map(|(width, height)| ImageDimensions { width, height });
            result.push((url, dimensions));
        }

        result
    }

    /// Music URL, if any.
    pub fn audio_url(&self) -> Option<&str> {
        self.music
            .as_ref()
            .and_then(|music| music.url.as_deref())
            .filter(|url| is_https_url(url))
    }
}

pub(crate) fn is_https_url(value: &str) -> bool {
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    if url.scheme() == "https" && url.host_str().is_some() {
        return true;
    }
    #[cfg(feature = "integration-tests")]
    {
        if url.scheme() == "http" && matches!(url.host_str(), Some("127.0.0.1" | "localhost")) {
            return true;
        }
    }
    false
}

/// Resolved media metadata for one post, ready to download.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaInfo {
    /// Post ID.
    pub post_id: String,
    /// Original user input.
    pub source_url: String,
    /// Canonical TikTok page URL.
    pub page_url: String,
    /// Author.
    pub author: Author,
    /// Caption.
    pub description: Option<String>,
    /// Selected video URL.
    pub video_url: Option<String>,
    /// Selected photo URLs in gallery order.
    pub photo_urls: Vec<String>,
    /// Dimensions for each photo URL, when known.
    pub photo_dimensions: Vec<Option<ImageDimensions>>,
    /// Music URL.
    pub audio_url: Option<String>,
    /// Whether the post is a photo slideshow.
    pub is_slideshow: bool,
    /// Whether TikTok marks the post as age-restricted.
    pub is_age_restricted: bool,
    /// Music metadata.
    pub music: Option<Music>,
    /// Available video bitrates.
    pub bitrates: Vec<Bitrate>,
    /// Default video width.
    pub video_width: Option<u32>,
    /// Default video height.
    pub video_height: Option<u32>,
    /// Default video duration in seconds.
    pub duration_sec: Option<u32>,
}

impl MediaInfo {
    /// Whether at least one downloadable media URL exists.
    pub fn has_media(&self) -> bool {
        self.video_url.is_some() || !self.photo_urls.is_empty() || self.audio_url.is_some()
    }
}

fn is_h264(codec: Option<&str>) -> bool {
    let Some(codec) = codec else {
        return false;
    };
    let codec = codec.to_ascii_lowercase();
    codec.contains("avc") || codec.contains("h264")
}

fn pick_image_url(urls: &[String]) -> Option<String> {
    urls.iter()
        .find(|url| {
            let lower = url.to_ascii_lowercase();
            lower.contains(".jpeg") || lower.contains(".jpg")
        })
        .or_else(|| {
            urls.iter().find(|url| {
                let lower = url.to_ascii_lowercase();
                lower.contains(".png") || lower.contains(".webp")
            })
        })
        .or_else(|| urls.first())
        .cloned()
}

// ---------------------------------------------------------------------------
// Raw deserialization types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct UniversalData {
    #[serde(rename = "__DEFAULT_SCOPE__")]
    pub(crate) default_scope: Option<DefaultScope>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct DefaultScope {
    #[serde(rename = "webapp.video-detail")]
    pub(crate) video_detail: Option<VideoDetail>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct VideoDetail {
    #[serde(rename = "statusMsg")]
    pub(crate) status_msg: Option<String>,
    #[serde(rename = "itemInfo")]
    pub(crate) item_info: Option<ItemInfo>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ItemInfo {
    #[serde(rename = "itemStruct")]
    pub(crate) item_struct: Option<ItemStruct>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ItemStruct {
    pub(crate) id: Option<String>,
    pub(crate) desc: Option<String>,
    pub(crate) author: Option<AuthorRaw>,
    pub(crate) video: Option<VideoRaw>,
    pub(crate) image_post: Option<ImagePostRaw>,
    pub(crate) music: Option<MusicRaw>,
    pub(crate) is_content_classified: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AuthorRaw {
    pub(crate) unique_id: Option<String>,
    pub(crate) nickname: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct VideoRaw {
    pub(crate) play_addr: Option<UrlValue>,
    pub(crate) download_addr: Option<UrlValue>,
    pub(crate) bitrate_info: Option<Vec<BitrateRaw>>,
    pub(crate) width: Option<u32>,
    pub(crate) height: Option<u32>,
    pub(crate) duration: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BitrateRaw {
    #[serde(rename = "Bitrate", alias = "bitrate")]
    pub(crate) bitrate: Option<u64>,
    #[serde(rename = "CodecType", alias = "codecType")]
    pub(crate) codec: Option<String>,
    #[serde(rename = "PlayAddr", alias = "playAddr")]
    pub(crate) play_addr: Option<UrlValue>,
    #[serde(rename = "Width", alias = "width")]
    pub(crate) width: Option<u32>,
    #[serde(rename = "Height", alias = "height")]
    pub(crate) height: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ImagePostRaw {
    #[serde(default)]
    pub(crate) images: Vec<ImageRaw>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ImageRaw {
    #[serde(rename = "imageURL", alias = "imageUrl")]
    pub(crate) image_url: Option<UrlValue>,
    #[serde(rename = "imageWidth", alias = "width")]
    pub(crate) width: Option<u32>,
    #[serde(rename = "imageHeight", alias = "height")]
    pub(crate) height: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MusicRaw {
    pub(crate) play_url: Option<UrlValue>,
    pub(crate) play_url_uri: Option<UrlValue>,
    pub(crate) title: Option<String>,
    pub(crate) author_name: Option<String>,
}

/// TikTok URL fields are sometimes a string, a list, or an object with a
/// `urlList`/`UrlList`/`url_list` array.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub(crate) enum UrlValue {
    String(String),
    List(Vec<String>),
    Object {
        #[serde(rename = "urlList", alias = "UrlList", alias = "url_list", default)]
        urls: Vec<String>,
    },
}

impl UrlValue {
    pub(crate) fn first(&self) -> Option<String> {
        self.all().into_iter().next()
    }

    pub(crate) fn all(&self) -> Vec<String> {
        match self {
            Self::String(value) => vec![value.clone()],
            Self::List(values) => values.clone(),
            Self::Object { urls } => urls.clone(),
        }
    }
}

impl ItemStruct {
    pub(crate) fn into_item(self, fallback_post_id: &str) -> Result<TikTokItem> {
        let post_id = self.id.unwrap_or_else(|| fallback_post_id.to_owned());
        let author = self
            .author
            .map(|author| Author {
                id: author.unique_id.unwrap_or_else(|| "unknown".into()),
                nickname: author.nickname,
            })
            .unwrap_or_else(|| Author {
                id: "unknown".into(),
                nickname: None,
            });

        let video = self.video.map(|video| VideoInfo {
            play_url: video.play_addr.and_then(|value| value.first()),
            download_url: video.download_addr.and_then(|value| value.first()),
            bitrates: video
                .bitrate_info
                .unwrap_or_default()
                .into_iter()
                .map(|bitrate| Bitrate {
                    bitrate: bitrate.bitrate.unwrap_or(0),
                    codec: bitrate.codec,
                    url: bitrate.play_addr.and_then(|value| value.first()),
                    width: bitrate.width,
                    height: bitrate.height,
                })
                .collect(),
            width: video.width,
            height: video.height,
            duration_sec: video.duration,
        });

        let photos = self
            .image_post
            .map(|image_post| {
                image_post
                    .images
                    .into_iter()
                    .filter_map(|image| {
                        image
                            .image_url
                            .map(|value| ImageInfo {
                                urls: value.all(),
                                width: image.width,
                                height: image.height,
                            })
                            .filter(|image| !image.urls.is_empty())
                    })
                    .collect()
            })
            .unwrap_or_default();

        let music = self.music.map(|music| Music {
            url: music
                .play_url
                .and_then(|value| value.first())
                .or_else(|| music.play_url_uri.and_then(|value| value.first())),
            title: music.title,
            author: music.author_name,
        });

        Ok(TikTokItem {
            post_id,
            author,
            description: self.desc,
            video,
            photos,
            music,
            is_content_classified: self.is_content_classified.unwrap_or(false),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item_with_bitrates() -> TikTokItem {
        TikTokItem {
            post_id: "1".into(),
            author: Author {
                id: "a".into(),
                nickname: None,
            },
            description: None,
            video: Some(VideoInfo {
                play_url: Some("https://cdn/play".into()),
                download_url: Some("https://cdn/download".into()),
                bitrates: vec![
                    Bitrate {
                        bitrate: 500,
                        codec: Some("bytevc1".into()),
                        url: Some("https://cdn/low-hevc".into()),
                        width: Some(720),
                        height: Some(1280),
                    },
                    Bitrate {
                        bitrate: 900,
                        codec: Some("h264".into()),
                        url: Some("https://cdn/high-h264".into()),
                        width: Some(720),
                        height: Some(1280),
                    },
                    Bitrate {
                        bitrate: 1_200,
                        codec: Some("bytevc1".into()),
                        url: Some("https://cdn/high-hevc".into()),
                        width: Some(1080),
                        height: Some(1920),
                    },
                ],
                width: Some(1080),
                height: Some(1920),
                duration_sec: Some(8),
            }),
            photos: vec![],
            music: None,
            is_content_classified: false,
        }
    }

    #[test]
    fn quality_policies_pick_expected_streams() {
        let item = item_with_bitrates();
        assert_eq!(
            item.best_video_url(QualityPolicy::Best),
            Some("https://cdn/high-hevc")
        );
        assert_eq!(
            item.best_video_url(QualityPolicy::Fast),
            Some("https://cdn/low-hevc")
        );
        assert_eq!(
            item.best_video_url(QualityPolicy::Compatible),
            Some("https://cdn/high-h264")
        );
        assert_eq!(
            item.best_video_url(QualityPolicy::Balanced),
            Some("https://cdn/high-h264")
        );
    }

    #[test]
    fn photos_are_deduped_and_prefer_jpeg() {
        let item = TikTokItem {
            post_id: "1".into(),
            author: Author {
                id: "a".into(),
                nickname: None,
            },
            description: None,
            video: None,
            photos: vec![
                ImageInfo {
                    urls: vec!["https://cdn/a.webp".into(), "https://cdn/a.jpeg?x=1".into()],
                    width: Some(1080),
                    height: Some(1440),
                },
                ImageInfo {
                    urls: vec!["https://cdn/a.jpeg?x=1".into()],
                    width: None,
                    height: None,
                },
            ],
            music: None,
            is_content_classified: false,
        };
        assert_eq!(item.photo_urls(), vec!["https://cdn/a.jpeg?x=1"]);
    }
}
