# ystt — user guide

`ystt` (**Yet Simple TikTok Toolkit**) downloads public TikTok videos and photo
galleries. The Rust library is the primary product; the CLI is deliberately small.

## Features

- public video posts, photo galleries and optional music;
- official embed-page fallback for videos when the full page is blocked;
- `vm.` / `vt.` / `/t/` short links;
- browser TLS/JA3/JA4 + HTTP/2 emulation via `wreq`;
- own parallel WAF proof-of-work solver with a solved-cookie cache;
- range-first video downloads, buffered writes, adaptive range concurrency;
- one-request `200` fast path for videos up to 64 MiB;
- content-addressed file cache (BLAKE3) with TTL and atomic manifests;
- persistent metadata/signed-URL cache across processes;
- batch API with bounded concurrency and per-item cancellation;
- in-memory video downloads with no media files written to disk;
- parallel range downloads assembled directly in memory;
- shared cache root across output directories;
- cross-process file-cache locks;
- resumable videos and filename templates;
- unified progress for downloads, resolve calls and batches;
- optional HTTP/2 connection/keep-alive tuning without changing TLS/JA3/JA4;
- progress events through a callback, channel or custom sink;
- partial gallery results with per-item failures;
- per-media retry budget;
- bitrate/codec/dimension metadata and four quality policies;
- typed errors, stage timings, offline tests and a local HTTP integration suite.

## Constraints

- ALPN Auto transport: HTTP/2 preferred, automatic HTTP/1.1 fallback for
  H1-only CDN hosts;
- no account cookies are accepted;
- no proxies in v1;
- no mobile API in v1;
- no private/deleted/login-only posts;
- no live streams;
- HTTP/3 was evaluated and intentionally not used (see `INTERNALS.md`).

The module keeps **server-set session cookies** from the same page response
(`ttwid`, `tt_chain_token`, `tt_csrf_token`). The media CDN requires them. When
the metadata cache is enabled they are persisted in a filtered, owner-only cache
entry for the signed-URL lifetime; account cookies are never accepted or stored.

### Transport policy

Default `TransportPolicy::Auto` lets ALPN select the protocol per host:

- page and `v16`/`v19` video CDNs negotiate **HTTP/2**;
- `v45` music and other H1-only hosts automatically use **HTTP/1.1**;
- no manual host lists, no failed H2 handshake, no downgrade flags.

Advanced overrides: `YsttBuilder::transport(TransportPolicy::Http2Only)` or
`Http1Only`. `Auto` is the production recommendation.

## Requirements

- Rust 1.99+ (edition 2024);
- Linux, macOS or Windows;
- HTTPS with HTTP/2 or HTTP/1.1 (HTTP/2 is preferred automatically);
- C toolchain, CMake, Perl and Clang for BoringSSL (`wreq`).

## Quick start

```bash
cargo build --release
./target/release/ystt "https://www.tiktok.com/@user/video/123" -o downloads
./target/release/ystt "https://www.tiktok.com/@user/photo/456" -o downloads
```

Metadata only:

```bash
./target/release/ystt --info "https://vm.tiktok.com/XXXX/"
```

### CLI flags

```text
ystt <URL> [-o DIR] [--info] [--json] [--quality best|compatible|balanced|fast]
           [--audio] [--no-cache] [--cache-dir DIR]
ystt --urls-file FILE [--concurrency N] [same download flags]
ystt --progress URL
ystt --filename "{author}-{post_id}-{quality}" URL
ystt --resume URL
```

Defaults:

- quality `balanced`;
- file and metadata caches enabled;
- content-addressed cache under `<output>/.ystt-cache`;
- metadata cache under the user cache directory;
- gallery concurrency 12;
- adaptive ranges enabled for files above 64 MiB;
- files at or below 64 MiB use one normal `200` GET;
- partial gallery results enabled.
- `--no-cache` disables both file and metadata caches for one call.
- `--cache-dir` shares file and metadata caches across output directories.
- `--json` writes `ErrorInfo` JSON to stderr when a command fails.
- `--urls-file` runs a batch from one URL per line (`#` comments and blank lines ignored; max 10,000 URLs / 8 MiB).
- `--concurrency` controls batch concurrency (default 4, capped at 64); it requires `--urls-file`.
- `--progress` writes one `ProgressEvent` JSON object per line to stderr.
- `--filename` sets the filename base; placeholders: `{author}`, `{post_id}`,
  `{quality}`. Path separators are sanitized.
- `--resume` continues a video from `.<name>.part` using a range request. It
  uses one resumable stream; adaptive range parallelism is skipped while enabled.

## Library

```rust
use ystt::{DownloadOptions, QualityPolicy, Ystt};

#[tokio::main]
async fn main() -> Result<(), ystt::YsttError> {
    let ystt = Ystt::builder()
        .output_dir("downloads")
        .quality(QualityPolicy::Balanced)
        .parallel_ranges(true)
        .max_connections(6)
        .photo_concurrency(12)
        .file_cache(true)
        .build()?;

    let result = ystt
        .download("https://www.tiktok.com/@user/video/123")
        .await?;

    if let Some(video) = result.video {
        println!("{} ({} bytes)", video.path.display(), video.size);
    }
    println!("photos: {}", result.photos.len());
    println!("failures: {}", result.failures.len());
    println!("cache hit: {}", result.cache_hit);
    println!("total: {} ms", result.timings.total_ms);
    Ok(())
}
```

### In-memory downloads

```rust
let video = ystt
    .download_video_bytes_with(url, DownloadOptions::new().max_bytes(49 * 1024 * 1024))
    .await?;
println!("{} bytes, {}", video.bytes.len(), video.mime);
```

No media or cache files are created. Use this path for Telegram bots, HTTP
streaming and other consumers that need the bytes immediately.

## Configuration groups

`YsttConfig` is grouped by responsibility:

```rust
use ystt::YsttConfig;

let mut config = YsttConfig::default();
config.output.dir = "downloads".into();
config.output.quality = ystt::QualityPolicy::Balanced;
config.media.photo_concurrency = 12;
config.cache.metadata = true;
config.network.http.retries = 2;
config.resolver.use_embed = false;
```

Groups: `output`, `media`, `cache`, `network`, `resolver`, `waf`.
The builder remains the preferred way to construct a downloader. Per-call
`DownloadOptions` contains only overrides; `BatchOptions` embeds one
`DownloadOptions` for every item and adds batch concurrency/progress/signals.

## JSON / web API

`MediaInfo`, `DownloadResult`, `Timings`, `DownloadedFile`, `ErrorInfo` and all progress events implement `serde::Serialize`. A typical Axum
handler is a thin wrapper:

```rust
use axum::{Json, response::IntoResponse};
use ystt::{DownloadOptions, Ystt, YsttError};

async fn download(
    ystt: Ystt,
    url: String,
) -> Result<Json<ystt::DownloadResult>, (u16, Json<ystt::ErrorInfo>)> {
    match ystt.download(&url).await {
        Ok(result) => Ok(Json(result)),
        Err(error) => {
            let status = error.status().unwrap_or(500);
            Err((status, Json(error.info())))
        }
    }
}
```

`error.code()` returns a stable snake_case code (`unavailable`, `network`,
`parse`, `cancelled`, ...). `Ystt` is cloneable and safe to share between
requests; its URL cache and file cache are bounded.

## Batch API

```rust
use ystt::{BatchOptions, Ystt};

let batch = ystt
    .download_many(urls, BatchOptions::new().concurrency(4).output_dir("downloads"))
    .await;
println!("ok={} failed={}", batch.successes(), batch.failures());
for item in batch.items {
    if let Some(result) = item.result {
        println!("{}: {} ms", item.url, result.timings.total_ms);
    } else if let Some(error) = item.error {
        println!("{}: {} ({})", item.url, error.code, error.message);
    }
}
```

Results preserve input order. Each item has either `result` or a stable
`ErrorInfo`; one failed URL does not cancel the remaining batch. `BatchOptions`
accepts `output_dir`, `quality`, `include_audio`, `file_cache`,
`metadata_cache`, `allow_partial`, a shared cancellation token and per-item
cancellation tokens. Concurrency defaults to 4 and is capped at 64.

```rust
let token = CancellationToken::new();
token.cancel();
let batch = ystt
    .download_many(urls, BatchOptions::new().item_signal(2, token))
    .await;
// only item 2 is cancelled
```

## HTTP/2 tuning

Default transport settings are the untouched Chrome 149 profile. The optional
`Http2Tuning` changes only connection-window and keep-alive behavior:

```rust
use std::time::Duration;
use ystt::{Http2Tuning, Ystt};

let ystt = Ystt::builder()
    .http2_tuning(Http2Tuning {
        connection_window: Some(32 * 1024 * 1024),
        keep_alive_interval: Some(Duration::from_secs(30)),
        ..Http2Tuning::default()
    })
    .build()?;
```

TLS/JA3/JA4, HTTP/2 SETTINGS order, pseudo-header order and stream dependency
remain exactly as provided by the browser emulation.

### For a web API

Recommended shape for a long-running backend:

```rust
use std::sync::Arc;
use std::time::Duration;
use ystt::{Http2Tuning, Ystt};

let ystt = Arc::new(
    Ystt::builder()
        .cache_dir("/var/cache/ystt")
        .http2_tuning(Http2Tuning {
            keep_alive_interval: Some(Duration::from_secs(30)),
            keep_alive_timeout: Some(Duration::from_secs(5)),
            keep_alive_while_idle: Some(true),
            ..Http2Tuning::default()
        })
        .build()?,
);
```

- Build one `Ystt` per process and clone the `Arc` into handlers.
- Keep `TransportPolicy::Auto`; it uses H2 for pages/video and H1 for `v45`
  music without failed handshakes.
- Keep `connection_window: None` unless a real deployment benchmark proves a
  larger window helps. The Chrome profile already uses a 15 MiB connection
  window.
- Use `keep_alive_*` for API processes that keep connections idle between jobs.
  It is irrelevant for a one-shot CLI.
- Share `cache_dir` between workers. File and metadata caches are atomic and
  safe for concurrent readers/writers.
- Do not create a new `Ystt` per request: that throws away the connection pool,
  TLS session cache, WAF cookie cache and signed-URL cache.
- `Http2Only` is an advanced override; do not use it when H1-only CDN hosts must
  remain supported.

The module does not measure CDN responses or collect per-host throughput data.
Use `RUST_LOG=debug` for protocol/request diagnostics and the stage timings in
`DownloadResult` for application-level latency.

## Progress events

One context-aware event stream is used by `download_with`, `resolve_with` and
`download_many`.

```rust
use ystt::{DownloadOptions, ProgressStage, progress_channel};

let (reporter, mut events) = progress_channel();
let options = DownloadOptions::new().progress(reporter);

let download = ystt.download_with(url, options);
tokio::pin!(download);

loop {
    tokio::select! {
        event = events.recv() => {
            if let Some(event) = event {
                match event.stage {
                    ProgressStage::MediaProgress { downloaded_bytes, total_bytes, .. } => {
                        println!("{downloaded_bytes}/{total_bytes:?}");
                    }
                    ProgressStage::Completed { total_ms } => println!("done in {total_ms} ms"),
                    ProgressStage::Failed { error } => eprintln!("{}: {}", error.code, error.message),
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
```

Every `ProgressEvent` has:

```text
job_id       unique ID for a solo download, resolve, or batch
url          item URL; None for batch-level events
batch_index  Some(index) for batch items, None for solo
batch_total  Some(total) for batch items
stage        typed ProgressStage
```

Stages: `BatchStarted`, `BatchProgress`, `BatchCompleted`, `Resolving`,
`Resolved`, `CacheHit`, `WafChallenge`, `WafSolved`, `MediaStarted`,
`MediaProgress`, `MediaCompleted`, `Completed`, `Failed`.

`BatchProgress` contains only aggregate item counts (`completed_items`,
`failed_items`, `total_items`), not CDN or network measurements.

For batches, attach the same reporter:

```rust
let batch = ystt
    .download_many(urls, BatchOptions::new().progress(reporter))
    .await;
```

`BatchStarted` and `BatchCompleted` are batch-level events; item events carry
`batch_index`/`batch_total`. A failed item emits `Failed` and does not cancel the
rest. `BatchResult` also provides final successes/failures and ordered per-item
results.

Any `Fn(ProgressEvent) + Send + Sync` can be used as a `ProgressSink` through
`DownloadOptions::progress` or `BatchOptions::progress`.

## File and metadata cache

The file cache is content-addressed:

```text
<output>/.ystt-cache/
├── manifests/<post_id>_<quality>_<audio>.json
└── objects/<blake3>.<ext>
```

With a shared cache root (`.cache_dir(...)` or `--cache-dir`):

```text
<cache>/
├── files/manifests/...
├── files/objects/...
└── metadata/<post_id>_<quality>.json
```

- first download copies bytes into the object store and writes a manifest;
- subsequent calls materialise a copy into the output directory without HTTP;
- file entries expire after `file_cache_ttl` (default 24 h);
- metadata entries expire after `metadata_cache_ttl`, capped by signed-URL expiry;
- metadata hits skip the TikTok page and reload the filtered media session cookies;
- failed/partial galleries are not cached;
- a per-key lock in `locks/` serialises concurrent processes for the same post;
  stale locks are reclaimed after 30 minutes;
- `Ystt::prune_file_cache()` removes stale manifests and unreferenced objects;
- `Ystt::prune_metadata_cache()` removes expired/malformed metadata;
- `Ystt::clear_file_cache()` and `clear_metadata_cache()` remove everything;
- `Ystt::invalidate_file_cache(post_id, quality, audio)` removes one manifest;
- `Ystt::invalidate_url_cache(post_id, quality)` drops one in-memory and disk entry;
- `--no-cache` disables both caches for one call.

The in-memory URL cache serves repeated calls in one process. The on-disk
metadata cache serves new processes; both use expiry-aware TTLs and are
invalidated together by `invalidate_url_cache`.

## Partial galleries

By default a failed photo does not discard the rest:

```rust
let result = ystt.download_with(url, DownloadOptions::new().allow_partial(true)).await?;
for failure in &result.failures {
    eprintln!("{} {}: {}", failure.kind.as_str(), failure.url, failure.error);
}
```

Set `.allow_partial(false)` to return `YsttError::Partial(...)`.

## Quality policies

| Policy | Meaning |
|---|---|
| `best` | highest bitrate |
| `compatible` | highest H.264/AVC |
| `balanced` | highest H.264 within 1080p/1920 px; best speed/size for a compatible file (default) |
| `fast` | lowest stream at least 360p |

`best` forces the highest bitrate (often H.265 and ~3x larger). `balanced` uses
the full page but selects H.264/`playAddr` (~1.1 MB in the test video). If the
source has only one resolution, every policy selects that source resolution; for
example the 200 s `@traveling_show` test video is 576×1024 at 1.27 Mbit/s H.264,
and there is no higher variant to request. The official embed page is available
as an opt-in/fallback path (`YsttBuilder::use_embed(true)`); it is smaller but
its H.264 URL was larger and slower in benchmarks, so it is not the default.

`--info` prints every bitrate with codec, dimensions and URL, plus gallery
dimensions.

## Errors

`YsttError` variants: `InvalidUrl`, `UnsupportedUrl`, `Config`, `Unavailable`,
`AgeRestricted`, `NoMedia`, `Network(NetError)`, `Waf(WafError)`, `Parse`,
`FileTooLarge`, `Io`, `Cancelled`, `Range`, `Partial(Vec<PartialFailure>)`,
`Internal`.

Helpers: `error.status()`, `error.code()`, `error.info()` and
`error.is_retryable()`. `ErrorInfo` is directly serializable for HTTP APIs.

## Testing

Offline tests (no network):

```bash
cargo test --workspace
```

Local HTTP pipeline integration tests (also offline, but feature-gated because
they deliberately use `http://127.0.0.1`):

```bash
cargo test --features integration-tests --test integration_pipeline
```

Manual real-network smoke is documented in `INTERNALS.md`. Do not run it in loops.

## Project layout

```text
ystt/
├── src/                  # root facade
├── crates/
│   ├── ystt-waf/         # challenge parser + parallel SHA-256 solver
│   ├── ystt-net/         # wreq client, retries, limits, responses
│   ├── ystt-core/        # URL, parser, model, pipeline, file cache
│   └── ystt-cli/         # minimal CLI
├── examples/             # download and benchmark examples
├── docs/                 # GUIDE.md + INTERNALS.md
└── tests/                # offline + local-HTTP tests
```
