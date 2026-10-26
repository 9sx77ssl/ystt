//! Unified progress example for a single download.
//!
//! Run:
//!   cargo run --release --example progress -- <url> [output-dir]

use ystt::{DownloadOptions, ProgressStage, Ystt, YsttError, progress_channel};

#[tokio::main]
async fn main() -> Result<(), YsttError> {
    let url = std::env::args()
        .nth(1)
        .ok_or_else(|| YsttError::InvalidUrl("missing URL".into()))?;
    let output = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "downloads".into());
    let ystt = Ystt::builder().output_dir(&output).build()?;
    let (reporter, mut events) = progress_channel();
    let download = ystt.download_with(
        &url,
        DownloadOptions::new()
            .output_dir(&output)
            .progress(reporter),
    );
    tokio::pin!(download);

    loop {
        tokio::select! {
            event = events.recv() => {
                if let Some(event) = event {
                    match event.stage {
                        ProgressStage::BatchStarted { total_items } => {
                            println!("batch started: {total_items}");
                        }
                        ProgressStage::MediaProgress { downloaded_bytes, total_bytes, .. } => {
                            println!("media {downloaded_bytes}/{total_bytes:?}");
                        }
                        ProgressStage::Completed { total_ms } => {
                            println!("completed in {total_ms} ms");
                        }
                        ProgressStage::Failed { error } => {
                            eprintln!("failed: {} ({})", error.code, error.message);
                        }
                        _ => {}
                    }
                }
            }
            result = &mut download => {
                result?;
                break;
            }
        }
    }
    Ok(())
}
