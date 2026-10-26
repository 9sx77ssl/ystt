//! Batch download example.
//!
//! Run:
//!   cargo run --release --example batch -- <url> <url> [output-dir]

use ystt::{BatchOptions, Ystt, YsttError};

#[tokio::main]
async fn main() -> Result<(), YsttError> {
    let mut args = std::env::args().skip(1);
    let first = args
        .next()
        .ok_or_else(|| YsttError::InvalidUrl("missing URL".into()))?;
    let mut urls = vec![first];
    let mut output = "downloads".to_owned();
    for arg in args {
        if arg.starts_with("http://") || arg.starts_with("https://") {
            urls.push(arg);
        } else {
            output = arg;
        }
    }

    let ystt = Ystt::builder().output_dir(&output).build()?;
    let batch = ystt
        .download_many(urls, BatchOptions::new().output_dir(&output).concurrency(4))
        .await;

    println!(
        "total={} ms ok={} failed={}",
        batch.total_ms,
        batch.successes(),
        batch.failures()
    );
    for item in batch.items {
        if let Some(result) = item.result {
            println!("ok {} {} ms", item.url, result.timings.total_ms);
        } else if let Some(error) = item.error {
            println!("error {} {} ({})", item.url, error.code, error.message);
        }
    }
    Ok(())
}
