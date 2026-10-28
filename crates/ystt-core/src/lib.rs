//! `ystt-core` — typed TikTok model, parser and download pipeline.
//!
//! Most users should depend on the root `ystt` crate, which re-exports this
//! crate together with `ystt-net` and `ystt-waf`.

pub mod cache;
mod cache_util;
pub mod config;
mod cookie;
pub mod download;
pub mod error;
pub mod file_cache;
pub mod media;
pub mod metadata;
pub mod model;
pub mod parse;
pub mod plan;
pub mod progress;
pub mod soundcloud;
pub mod soundcloud_cache;
pub mod url;

pub use cache::{UrlCache, signed_url_ttl};
pub use config::{DownloadOptions, FilenameCollision, QualityPolicy, YsttConfig};
pub use download::{
    BatchItemResult, BatchOptions, BatchResult, DownloadResult, InMemoryFile, InMemoryPost,
    InMemoryVideo, Timings, Ystt, YsttBuilder,
};
pub use error::{ErrorContext, ErrorInfo, PartialFailure, Result, YsttError};
pub use file_cache::{CachedFile, FileCache, FileCacheEntry};
pub use media::{DownloadedFile, sanitize_filename};
pub use metadata::{CachedMetadata, MetadataCache, default_metadata_root, filter_session_cookie};
pub use model::{
    Author, Bitrate, ImageInfo, MediaInfo, MediaKind, Music, Provider, TikTokItem, VideoInfo,
};
pub use parse::{PageParse, extract_universal_json, parse_page};
pub use plan::{ByteRange, RangePlan, RangePlanner};
pub use progress::{
    MediaTarget, ProgressEvent, ProgressReporter, ProgressSink, ProgressStage, progress_channel,
};
pub use soundcloud::{
    SoundCloudArtwork, SoundCloudAudio, SoundCloudClient, SoundCloudEntity, SoundCloudPlaylist,
    SoundCloudPlaylistAudio, SoundCloudTrack, SoundCloudTrackFailure, SoundCloudTranscoding,
    is_soundcloud_url, normalize_url as normalize_soundcloud_url,
};
pub use soundcloud_cache::SoundCloudCache;
pub use tokio_util::sync::CancellationToken;
pub use url::{TikTokUrl, find_post_id_in_html, get_post_id};
pub use ystt_net::BrowserProfile;
