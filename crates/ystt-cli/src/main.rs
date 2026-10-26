//! Minimal `ystt` command-line interface.
//!
//! The library is the product; this binary exists for quick manual downloads and
//! metadata inspection. Only flags that change user-visible behaviour are kept.

use std::path::PathBuf;

use clap::{Parser, ValueEnum};
use tracing_subscriber::EnvFilter;
use ystt_core::{DownloadOptions, DownloadResult, QualityPolicy, Ystt, YsttError};

#[derive(Debug, Parser)]
#[command(
    name = "ystt",
    version,
    about = "Fast TikTok video and photo-gallery downloader",
    long_about = "Download a public TikTok video or every photo of a gallery. No account cookies, no proxies, no mobile API."
)]
struct Cli {
    /// TikTok URL.
    url: String,

    /// Output directory.
    #[arg(short = 'o', long, default_value = "downloads")]
    output: PathBuf,

    /// Print metadata and do not download.
    #[arg(long)]
    info: bool,

    /// Print JSON instead of human-readable text.
    #[arg(long)]
    json: bool,

    /// Video quality policy.
    #[arg(long, value_enum, default_value_t = QualityArg::Balanced)]
    quality: QualityArg,

    /// Download the post's music track too.
    #[arg(long)]
    audio: bool,

    /// Do not use the content-addressed file cache.
    #[arg(long)]
    no_cache: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum QualityArg {
    Best,
    Compatible,
    Balanced,
    Fast,
}

impl From<QualityArg> for QualityPolicy {
    fn from(value: QualityArg) -> Self {
        match value {
            QualityArg::Best => Self::Best,
            QualityArg::Compatible => Self::Compatible,
            QualityArg::Balanced => Self::Balanced,
            QualityArg::Fast => Self::Fast,
        }
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    init_tracing();

    if let Err(error) = run(cli).await {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<(), YsttError> {
    let ystt = Ystt::builder()
        .output_dir(&cli.output)
        .quality(cli.quality.into())
        .include_audio(cli.audio)
        .file_cache(!cli.no_cache)
        .build()?;

    let options = DownloadOptions::new()
        .output_dir(&cli.output)
        .file_cache(!cli.no_cache);

    if cli.info {
        let info = ystt.resolve_with(&cli.url, &options).await?;
        if cli.json {
            print_json(&info)?;
        } else {
            print_info(&info);
        }
        return Ok(());
    }

    let result = ystt.download_with(&cli.url, options).await?;
    if cli.json {
        print_json(&result)?;
    } else {
        print_human(&result);
    }
    Ok(())
}

fn print_json<T: serde::Serialize>(value: &T) -> Result<(), YsttError> {
    let json =
        serde_json::to_string_pretty(value).map_err(|error| YsttError::Parse(error.to_string()))?;
    println!("{json}");
    Ok(())
}

fn print_info(info: &ystt_core::MediaInfo) {
    println!("post: {}", info.post_id);
    println!("author: @{}", info.author.id);
    println!("page: {}", info.page_url);
    println!("video: {}", info.video_url.as_deref().unwrap_or("-"));
    if let Some(width) = info.video_width
        && let Some(height) = info.video_height
    {
        println!("video_size: {width}x{height}");
    }
    if let Some(duration) = info.duration_sec {
        println!("duration: {duration}s");
    }
    for bitrate in &info.bitrates {
        println!(
            "bitrate: {} bps, codec={}, size={}x{}, url={}",
            bitrate.bitrate,
            bitrate.codec.as_deref().unwrap_or("-"),
            bitrate.width.map_or("-".to_owned(), |v| v.to_string()),
            bitrate.height.map_or("-".to_owned(), |v| v.to_string()),
            bitrate.url.as_deref().unwrap_or("-")
        );
    }
    println!("photos: {}", info.photo_urls.len());
    for (index, (url, dimensions)) in info
        .photo_urls
        .iter()
        .zip(info.photo_dimensions.iter())
        .enumerate()
    {
        let size = dimensions
            .map(|d| format!("{}x{}", d.width, d.height))
            .unwrap_or_else(|| "-".into());
        println!("photo[{}]: {} {}", index + 1, size, url);
    }
    println!("audio: {}", info.audio_url.as_deref().unwrap_or("-"));
    println!("slideshow: {}", info.is_slideshow);
    println!("age_restricted: {}", info.is_age_restricted);
}

fn print_human(result: &DownloadResult) {
    println!("post: {}", result.post_id);
    println!("author: @{}", result.author.id);
    println!("page: {}", result.page_url);
    println!("cache_hit: {}", result.cache_hit);

    if let Some(video) = &result.video {
        println!(
            "video: {} ({})",
            video.path.display(),
            human_size(video.size)
        );
    }
    for photo in &result.photos {
        println!(
            "photo: {} ({})",
            photo.path.display(),
            human_size(photo.size)
        );
    }
    if let Some(audio) = &result.audio {
        println!(
            "audio: {} ({})",
            audio.path.display(),
            human_size(audio.size)
        );
    }
    for failure in &result.failures {
        println!(
            "failed: {} {} ({})",
            failure.kind.as_str(),
            failure.url,
            failure.error
        );
    }

    println!(
        "timings: total {} ms | resolve {} ms | page {} ms | waf {} ms | parse {} ms | media {} ms",
        result.timings.total_ms,
        result.timings.resolve_ms,
        result.timings.page_ms,
        result.timings.waf_ms,
        result.timings.parse_ms,
        result.timings.media_ms
    );
}

fn human_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let bytes = bytes as f64;
    if bytes >= GIB {
        format!("{:.2} GiB", bytes / GIB)
    } else if bytes >= MIB {
        format!("{:.2} MiB", bytes / MIB)
    } else if bytes >= KIB {
        format!("{:.2} KiB", bytes / KIB)
    } else {
        format!("{bytes:.0} B")
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}
