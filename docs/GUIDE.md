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

The module does keep **server-set session cookies** from the same page response
(`ttwid`, `tt_chain_token`, `tt_csrf_token`) in memory. The media CDN requires them.
They are never persisted or logged.

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
           [--audio] [--no-cache]
```

Defaults:

- quality `balanced`;
- file cache enabled;
- content-addressed cache under `<output>/.ystt-cache`;
- gallery concurrency 12;
- adaptive ranges enabled for files above 64 MiB;
- files at or below 64 MiB use one normal `200` GET;
- partial gallery results enabled.

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

## JSON / web API

`MediaInfo`, `DownloadResult`, `Timings`, `DownloadedFile`, `ErrorInfo` and all
progress events implement `serde::Serialize`. A typical Axum handler is a thin
wrapper:

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

## Progress events

```rust
use ystt::{DownloadOptions, ProgressEvent, Ystt, progress_channel};

let (reporter, mut events) = progress_channel();
let options = DownloadOptions::new().progress(reporter);

let download = ystt.download_with(url, options);
tokio::pin!(download);

loop {
    tokio::select! {
        event = events.recv() => {
            if let Some(event) = event {
                match event {
                    ProgressEvent::MediaProgress { downloaded_bytes, total_bytes, .. } => {
                        println!("{downloaded_bytes}/{total_bytes:?}");
                    }
                    ProgressEvent::Completed { total_ms } => println!("done in {total_ms} ms"),
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

Events: `Resolving`, `Resolved`, `CacheHit`, `WafChallenge`, `WafSolved`,
`MediaStarted`, `MediaProgress`, `MediaCompleted`, `Completed`.

You can also pass any `Fn(ProgressEvent) + Send + Sync` via
`DownloadOptions::progress`.

## File cache

The cache is content-addressed:

```text
<output>/.ystt-cache/
├── manifests/<post_id>_<quality>_<audio>.json
└── objects/<blake3>.<ext>
```

- first download copies bytes into the object store and writes a manifest;
- subsequent calls materialise a copy into the output directory without HTTP;
- entries expire after `file_cache_ttl` (default 24 h);
- failed/partial galleries are not cached;
- `Ystt::prune_file_cache()` removes stale manifests and unreferenced objects;
- `Ystt::clear_file_cache()` removes everything;
- `Ystt::invalidate_file_cache(post_id, quality, audio)` removes one manifest;
- `Ystt::invalidate_url_cache(post_id, quality)` drops one signed-URL entry;
- `--no-cache` disables the file cache for one call.

The URL cache is separate: it stores signed CDN URLs in memory with expiry-aware
TTL, so a repeat download in the same process skips the page request.

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
