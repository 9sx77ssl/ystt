//! Minimal library example.
//!
//! Run:
//!   cargo run --example download -- "https://www.tiktok.com/@user/video/123" [output-dir]

use ystt::{DownloadOptions, Ystt, YsttError};

#[tokio::main]
async fn main() -> Result<(), YsttError> {
    let url = std::env::args()
        .nth(1)
        .ok_or_else(|| YsttError::InvalidUrl("missing URL argument".into()))?;
    let output = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "downloads".to_owned());

    let ystt = Ystt::builder()
        .output_dir(&output)
        .parallel_ranges(true)
        .max_connections(6)
        .build()?;

    let result = ystt
        .download_with(&url, DownloadOptions::new().output_dir(&output))
        .await?;

    if let Some(video) = &result.video {
        println!("video: {} ({} bytes)", video.path.display(), video.size);
    }
    for photo in &result.photos {
        println!("photo: {} ({} bytes)", photo.path.display(), photo.size);
    }
    println!("total: {} ms", result.timings.total_ms);
    Ok(())
}
