//! `ystt-core` — typed TikTok model, parser and download pipeline.
//!
//! Most users should depend on the root `ystt` crate, which re-exports this
//! crate together with `ystt-net` and `ystt-waf`.

pub mod cache;
pub mod config;
pub mod download;
pub mod error;
pub mod file_cache;
pub mod media;
pub mod metadata;
pub mod metrics;
pub mod model;
pub mod parse;
pub mod plan;
pub mod progress;
pub mod url;

pub use cache::{UrlCache, signed_url_ttl};
pub use config::{DownloadOptions, QualityPolicy, YsttConfig};
pub use download::{
    BatchItemResult, BatchOptions, BatchResult, DownloadResult, Timings, Ystt, YsttBuilder,
};
pub use error::{ErrorInfo, PartialFailure, Result, YsttError};
pub use file_cache::{CachedFile, FileCache, FileCacheEntry};
pub use media::{DownloadedFile, sanitize_filename};
pub use metadata::{CachedMetadata, MetadataCache, default_metadata_root, filter_session_cookie};
pub use metrics::{TransferKind, TransferMetric};
pub use model::{Author, Bitrate, ImageInfo, MediaInfo, MediaKind, Music, TikTokItem, VideoInfo};
pub use parse::{PageParse, extract_universal_json, parse_page};
pub use plan::{ByteRange, RangePlan, RangePlanner};
pub use progress::{MediaTarget, ProgressEvent, ProgressReporter, ProgressSink, progress_channel};
pub use url::{TikTokUrl, find_post_id_in_html, get_post_id};
