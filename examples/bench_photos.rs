//! Photo-gallery concurrency benchmark.
//!
//! Run:
//!   cargo run --release --example bench_photos -- <url> <output-dir> [concurrency]

use ystt::{DownloadOptions, Ystt, YsttError};

#[tokio::main]
async fn main() -> Result<(), YsttError> {
    let url = std::env::args()
        .nth(1)
        .ok_or_else(|| YsttError::InvalidUrl("missing URL".into()))?;
    let output = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "downloads".into());
    let concurrency = std::env::args()
        .nth(3)
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(4);

    let ystt = Ystt::builder()
        .output_dir(&output)
        .photo_concurrency(concurrency)
        .file_cache(false)
        .build()?;

    let result = ystt
        .download_with(&url, DownloadOptions::new().output_dir(&output))
        .await?;
    println!(
        "concurrency={concurrency} total_ms={} media_ms={} photos={} failures={}",
        result.timings.total_ms,
        result.timings.media_ms,
        result.photos.len(),
        result.failures.len()
    );
    Ok(())
}
