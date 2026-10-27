//! Video transport benchmark.
//!
//! Run:
//!   cargo run --release --example bench_video -- <url> <dir> [range|single|full] [chrome149|chrome131|firefox151|safari] [first_chunk] [min_chunk] [connections] [adaptive|fixed] [auto|http2|http1]

use ystt::{BrowserProfile, DownloadOptions, TransportPolicy, Ystt, YsttError};

#[tokio::main]
async fn main() -> Result<(), YsttError> {
    if let Ok(filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .try_init();
    }

    let url = std::env::args()
        .nth(1)
        .ok_or_else(|| YsttError::InvalidUrl("missing URL".into()))?;
    let output = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "downloads".into());
    let mode = std::env::args().nth(3).unwrap_or_else(|| "range".into());
    let browser: BrowserProfile = std::env::args()
        .nth(4)
        .unwrap_or_else(|| "chrome149".into())
        .parse()
        .map_err(YsttError::Config)?;
    let first_chunk = std::env::args()
        .nth(5)
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(8 * 1024 * 1024);
    let min_chunk = std::env::args()
        .nth(6)
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(2 * 1024 * 1024);
    let connections = std::env::args()
        .nth(7)
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(6);
    let adaptive = std::env::args().nth(8).is_none_or(|value| value != "fixed");
    let transport: TransportPolicy = std::env::args()
        .nth(9)
        .unwrap_or_else(|| "auto".into())
        .parse()
        .map_err(YsttError::Config)?;

    let ystt = Ystt::builder()
        .transport(transport)
        .output_dir(&output)
        .parallel_ranges(!mode.contains("single"))
        .browser(browser)
        .use_embed(!mode.contains("full"))
        .max_connections(connections)
        .adaptive_ranges(adaptive)
        .range_start_connections(connections)
        .range_first_chunk(first_chunk)
        .range_min_chunk(min_chunk)
        .range_threshold(if mode.contains("range") || mode == "full" {
            0
        } else {
            u64::MAX
        })
        .file_cache(false)
        .metadata_cache(false)
        .build()?;

    let result = ystt
        .download_with(&url, DownloadOptions::new().output_dir(&output))
        .await?;
    println!(
        "mode={mode} browser={} transport={} total_ms={} page_ms={} media_ms={} size={}",
        browser.as_str(),
        transport.as_str(),
        result.timings.total_ms,
        result.timings.page_ms,
        result.timings.media_ms,
        result.video.map(|video| video.size).unwrap_or(0)
    );
    Ok(())
}
