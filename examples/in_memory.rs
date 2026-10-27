//! Download one video into memory without touching the filesystem.
//!
//! ```bash
//! cargo run --release --example in_memory -- '<tiktok url>'
//! ```

use std::error::Error;

use ystt::{DownloadOptions, Ystt};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let url = std::env::args()
        .nth(1)
        .ok_or("usage: in_memory <tiktok-url>")?;
    let ystt = Ystt::builder().file_cache(false).build()?;
    let video = ystt
        .download_video_bytes_with(
            &url,
            DownloadOptions::new()
                .metadata_cache(true)
                .max_bytes(49 * 1024 * 1024),
        )
        .await?;
    println!(
        "{} bytes, {}, {}x{}, {}s",
        video.bytes.len(),
        video.mime,
        video.width.unwrap_or(0),
        video.height.unwrap_or(0),
        video.duration_sec.unwrap_or(0)
    );
    Ok(())
}
