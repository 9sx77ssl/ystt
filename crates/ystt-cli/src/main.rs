//! Minimal `ystt` command-line interface.
//!
//! The library is the product; this binary exists for quick manual downloads and
//! metadata inspection. Only flags that change user-visible behaviour are kept.

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, ValueEnum};
use tracing_subscriber::EnvFilter;
use ystt_core::{
    BatchOptions, BatchResult, DownloadOptions, DownloadResult, ProgressEvent, ProgressReporter,
    QualityPolicy, Ystt, YsttError,
};

#[derive(Debug, Parser)]
#[command(
    name = "ystt",
    version,
    about = "Fast TikTok video and photo-gallery downloader",
    long_about = "Download a public TikTok video or every photo of a gallery. No account cookies, no proxies, no mobile API."
)]
struct Cli {
    /// TikTok URL.
    #[arg(required_unless_present = "urls_file")]
    url: Option<String>,

    /// File with one TikTok URL per line (batch mode).
    #[arg(long, value_name = "FILE", conflicts_with_all = ["url", "info"])]
    urls_file: Option<PathBuf>,

    /// Batch concurrency (default 4, max 64).
    #[arg(long, default_value_t = 4)]
    concurrency: usize,

    /// Output directory.
    #[arg(short = 'o', long, default_value = "downloads")]
    output: PathBuf,

    /// Print metadata and do not download.
    #[arg(long)]
    info: bool,

    /// Print JSON instead of human-readable text.
    #[arg(long)]
    json: bool,

    /// Emit unified progress events as JSON lines on stderr.
    #[arg(long)]
    progress: bool,

    /// Video quality policy.
    #[arg(long, value_enum, default_value_t = QualityArg::Balanced)]
    quality: QualityArg,

    /// Output filename template: {author}, {post_id}, {quality}.
    #[arg(long, value_name = "TEMPLATE")]
    filename: Option<String>,

    /// Download the post's music track too.
    #[arg(long)]
    audio: bool,

    /// Resume interrupted videos from `.part` files.
    #[arg(long)]
    resume: bool,

    /// Do not use the content-addressed file cache or metadata cache.
    #[arg(long)]
    no_cache: bool,

    /// Shared cache root for file and metadata caches across outputs.
    #[arg(long, value_name = "DIR")]
    cache_dir: Option<PathBuf>,
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
    let json = cli.json;
    init_tracing();

    if let Err(error) = run(cli).await {
        if json {
            match serde_json::to_string(&error.info()) {
                Ok(payload) => eprintln!("{payload}"),
                Err(_) => eprintln!("error: {error}"),
            }
        } else {
            eprintln!("error: {error}");
        }
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<(), YsttError> {
    let mut builder = Ystt::builder()
        .output_dir(&cli.output)
        .quality(cli.quality.into())
        .include_audio(cli.audio)
        .resume(cli.resume)
        .file_cache(!cli.no_cache)
        .metadata_cache(!cli.no_cache);
    if let Some(cache_dir) = &cli.cache_dir {
        builder = builder.cache_dir(cache_dir);
    }
    if let Some(template) = &cli.filename {
        builder = builder.filename_template(template);
    }
    let ystt = builder.build()?;

    let reporter = cli_reporter(cli.progress);
    let mut options = DownloadOptions::new()
        .output_dir(&cli.output)
        .resume(cli.resume)
        .file_cache(!cli.no_cache)
        .metadata_cache(!cli.no_cache);
    if let Some(template) = &cli.filename {
        options = options.filename_template(template);
    }
    if let Some(reporter) = reporter.clone() {
        options = options.progress(reporter);
    }

    if let Some(path) = &cli.urls_file {
        let urls = read_urls_file(path)?;
        let mut batch_options = BatchOptions::new()
            .output_dir(&cli.output)
            .quality(cli.quality.into())
            .include_audio(cli.audio)
            .resume(cli.resume)
            .file_cache(!cli.no_cache)
            .metadata_cache(!cli.no_cache)
            .concurrency(cli.concurrency);
        if let Some(template) = &cli.filename {
            batch_options = batch_options.filename_template(template);
        }
        if let Some(reporter) = reporter {
            batch_options = batch_options.progress(reporter);
        }
        let batch = ystt.download_many(urls, batch_options).await;
        if cli.json {
            print_json(&batch)?;
        } else {
            print_batch_human(&batch);
        }
        return Ok(());
    }

    let url = cli
        .url
        .as_deref()
        .ok_or_else(|| YsttError::InvalidUrl("missing URL".into()))?;
    if cli.info {
        let info = ystt.resolve_with(url, &options).await?;
        if cli.json {
            print_json(&info)?;
        } else {
            print_info(&info);
        }
        return Ok(());
    }

    let result = ystt.download_with(url, options).await?;
    if cli.json {
        print_json(&result)?;
    } else {
        print_human(&result);
    }
    Ok(())
}

fn read_urls_file(path: &std::path::Path) -> Result<Vec<String>, YsttError> {
    const MAX_URL_FILE_BYTES: u64 = 8 * 1024 * 1024;
    const MAX_URLS: usize = 10_000;
    let metadata = std::fs::metadata(path)?;
    if metadata.len() > MAX_URL_FILE_BYTES {
        return Err(YsttError::Config(format!(
            "URL file is larger than {MAX_URL_FILE_BYTES} bytes"
        )));
    }
    let text = std::fs::read_to_string(path)?;
    let urls = parse_urls(&text);
    if urls.is_empty() {
        return Err(YsttError::Config("URL file is empty".into()));
    }
    if urls.len() > MAX_URLS {
        return Err(YsttError::Config(format!(
            "URL file contains more than {MAX_URLS} URLs"
        )));
    }
    Ok(urls)
}

fn cli_reporter(enabled: bool) -> Option<ProgressReporter> {
    enabled.then(|| {
        Arc::new(|event: ProgressEvent| match serde_json::to_string(&event) {
            Ok(line) => eprintln!("{line}"),
            Err(error) => eprintln!("progress serialization error: {error}"),
        }) as ProgressReporter
    })
}

fn parse_urls(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

fn print_batch_human(batch: &BatchResult) {
    println!(
        "batch: total {} ms | ok {} | failed {}",
        batch.total_ms,
        batch.successes(),
        batch.failures()
    );
    for item in &batch.items {
        if let Some(result) = &item.result {
            println!("ok: {} ({} ms)", item.url, result.timings.total_ms);
        } else if let Some(error) = &item.error {
            println!("error: {} {} ({})", item.url, error.code, error.message);
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_file_skips_comments_and_blank_lines() {
        let urls = parse_urls("# comment\n\nhttps://a\n  https://b  \n");
        assert_eq!(urls, vec!["https://a", "https://b"]);
    }
}
