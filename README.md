# ystt

**Yet Simple TikTok Toolkit** is a focused Rust library and CLI for downloading
public TikTok videos, photo galleries and music tracks with as little protocol
overhead as possible.

The project is deliberately small: one typed pipeline, no browser automation, no
account cookies, no proxies and no mobile-API dependency. It resolves short
links, reads TikTok's rehydration JSON, solves the WAF proof-of-work challenge
when required, negotiates the best HTTP protocol per CDN host, and writes media
through a content-addressed cache.

## Features

- Public videos, photo galleries and optional music.
- Full-page H.264 `playAddr` path with a four-policy quality selector.
- Official embed resolver as a fallback.
- `vm.`, `vt.` and `/t/` short links.
- Browser TLS/JA3/JA4 and HTTP/2 emulation via `wreq`.
- ALPN Auto transport: HTTP/2 where available, HTTP/1.1 fallback for H1-only
  CDN hosts such as `v45` music.
- Parallel WAF proof-of-work solver with an in-process solved-cookie cache.
- One-request `200` path for typical videos; adaptive ranges for large files.
- BLAKE3 content-addressed file cache with TTL and atomic manifests.
- Persistent metadata/signed-URL cache across processes.
- Batch API with bounded concurrency and per-item cancellation.
- In-memory video downloads for bots and streaming services, with no media
  files written to disk.
- Optional shared cache root across output directories.
- Cross-process file-cache locks.
- Resumable videos and configurable filename templates.
- Optional HTTP/2 connection/keep-alive tuning without changing the TLS/JA3/JA4
  or HTTP/2 settings fingerprint.
- Expiry-aware signed-URL cache.
- Unified typed progress for solo downloads and batches.
- Stage timings.
- Partial gallery results with per-item retry control.
- JSON-ready results and stable `ErrorInfo` payloads for HTTP APIs.
- No account cookies; only a filtered set of server session cookies is kept for
  the media CDN and, when the metadata cache is enabled, persisted with 0600
  permissions for the short signed-URL lifetime.

## Requirements

- Rust 1.99+ (edition 2024).
- Linux, macOS or Windows.
- A C toolchain, CMake, Perl and Clang for BoringSSL.

## Build

```bash
cargo build --release
```

The binary is `target/release/ystt`.

## CLI

```bash
ystt <URL> [-o DIR] [--info] [--json] [--quality best|compatible|balanced|fast]
           [--audio] [--no-cache] [--cache-dir DIR]
ystt --urls-file FILE [--concurrency N] [same download flags]
ystt --progress URL    # JSON-lines progress on stderr
ystt --filename "{author}-{post_id}-{quality}" URL
ystt --resume URL    # continue a video from a deterministic .part file
```

Examples:

```bash
ystt "https://www.tiktok.com/@user/video/123" -o downloads
ystt --audio "https://www.tiktok.com/@user/video/123" -o downloads
ystt "https://www.tiktok.com/@user/photo/456" -o downloads
ystt --info --json "https://vm.tiktok.com/XXXX/"
ystt --urls-file urls.txt --concurrency 8 -o downloads --json
```

Defaults: quality `balanced`, file and metadata caches enabled, 12 photo workers,
adaptive ranges for files over 64 MiB. `--no-cache` disables both caches;
`--cache-dir` shares the file and metadata caches across output directories.
With `--json`, failures are emitted as a structured `ErrorInfo` object on stderr.
`--progress` emits the unified `ProgressEvent` stream as JSON lines on stderr.

## Library

```rust
use ystt::{DownloadOptions, QualityPolicy, Ystt};

#[tokio::main]
async fn main() -> Result<(), ystt::YsttError> {
    let ystt = Ystt::builder()
        .output_dir("downloads")
        .quality(QualityPolicy::Balanced)
        .include_audio(false)
        .file_cache(true)
        .build()?;

    let result = ystt
        .download("https://www.tiktok.com/@user/video/123")
        .await?;

    if let Some(video) = &result.video {
        println!("{} ({} bytes)", video.path.display(), video.size);
    }
    println!("total: {} ms", result.timings.total_ms);
    Ok(())
}
```

### In-memory video

```rust
use ystt::{DownloadOptions, Ystt};

let ystt = Ystt::builder()
    .file_cache(false)
    .metadata_cache(false)
    .build()?;
let video = ystt
    .download_video_bytes_with(url, DownloadOptions::new().max_bytes(49 * 1024 * 1024))
    .await?;
send_to_telegram(video.bytes, &video.mime, video.duration_sec).await;
```

`download_video_bytes` / `download_video_bytes_with` resolve the post and return
the complete video as `bytes::Bytes`. They never create media or cache files;
metadata caching can be enabled independently. `InMemoryVideo` also exposes the
post ID, author, dimensions, duration and stage timings.

`Ystt` is cloneable and safe to share between async tasks. `MediaInfo`,
`DownloadResult`, `Timings`, `DownloadedFile` and `ErrorInfo`
implement `serde::Serialize`.

### Progress

One event stream covers solo downloads and batch items:

```rust
use ystt::{DownloadOptions, ProgressStage, progress_channel};

let (reporter, mut events) = progress_channel();
let handle = tokio::spawn(async move {
    while let Some(event) = events.recv().await {
        match event.stage {
            ProgressStage::MediaProgress { downloaded_bytes, total_bytes, .. } => {
                println!("{} {:?}/{:?}", event.url.unwrap_or_default(), downloaded_bytes, total_bytes);
            }
            ProgressStage::BatchCompleted { successes, failures, .. } => {
                println!("batch done: {successes} ok, {failures} failed");
            }
            _ => {}
        }
    }
});
let result = ystt.download_with(url, DownloadOptions::new().progress(reporter)).await?;
```

Every event carries `job_id`, optional `url`, and for batch items
`batch_index` / `batch_total`. Batch lifecycle is reported through `BatchStarted`, per-item
`BatchProgress` (completed/failed counts) and `BatchCompleted`; item failures are
`Failed` with stable `ErrorInfo`. `BatchOptions::progress(reporter)` attaches the same reporter to
`download_many`.

### Batch

```rust
use ystt::{BatchOptions, Ystt};

let batch = ystt
    .download_many(
        [
            "https://www.tiktok.com/@user/video/111",
            "https://www.tiktok.com/@user/video/222",
        ],
        BatchOptions::new()
            .concurrency(4)
            .output_dir("downloads")
            .include_audio(true),
    )
    .await;

println!("ok={} failed={}", batch.successes(), batch.failures());
for item in batch.items {
    match (item.result, item.error) {
        (Some(result), _) => println!("{}: {} ms", item.url, result.timings.total_ms),
        (_, Some(error)) => println!("{}: {} ({})", item.url, error.code, error.message),
        _ => {}
    }
}
```

Batch results preserve input order and include per-item errors; one failure does
not cancel other downloads.

## Transport

The default `TransportPolicy::Auto` lets ALPN choose per host:

- TikTok pages and the `v16`/`v19` video CDNs use HTTP/2.
- The `v45` music CDN and other H1-only hosts use HTTP/1.1.
- There is no manual host list and no failed HTTP/2 handshake before fallback.

`Http2Only` and `Http1Only` are available as explicit advanced overrides.
Production builds require HTTPS. Server session cookies are kept in memory and,
when the metadata cache is enabled, only the three media-CDN cookies are stored
with owner-only permissions for the signed-URL lifetime. Account cookies are
never accepted.

`Http2Tuning` can adjust the connection window and keep-alive values while
keeping the browser's TLS/JA3/JA4 and HTTP/2 SETTINGS fingerprint intact. It is
opt-in; the default is the untouched Chrome 149 profile.

## Performance

Measured on x86_64 Linux, release build, no proxy and no account cookies:

| Workload | ystt | Bun reference |
|---|---:|---:|
| 30.35 MiB video, cold | ~2.8–3.3 s | ~2.6–4.2 s |
| 30.35 MiB video, warm file cache | **0 ms** | ~2.8 s |
| 23-photo gallery, 12 workers | ~0.82–1.25 s | ~1.00–1.55 s |
| small video, cold | 774.8 ms | 824.5 ms |

Cold single-file downloads are dominated by the TikTok page TTFB and CDN
throughput, so expect parity with other clients on the same edge. The cache and
gallery paths are the deterministic wins. Peak RSS for a cold 30 MiB download is
about 17 MiB.

## Verification

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test --features integration-tests --test integration_pipeline
```

The default suite is offline. The integration suite starts a local HTTP server
and covers video ranges, session cookies, music, progress events, shared file
cache, persistent metadata cache, batch ordering/errors, unified progress,
partial galleries and the embed fallback.

## Layout

```text
crates/ystt-waf/    challenge parser and parallel SHA-256 solver
crates/ystt-net/    wreq client, ALPN transport, retries, host limits
crates/ystt-core/   URL model, parser, pipeline, media, caches
crates/ystt-cli/    minimal CLI
docs/               user guide and engineering internals
examples/           download and benchmark examples
tests/              offline and local-HTTP integration tests
```

## Documentation

- [`docs/GUIDE.md`](docs/GUIDE.md): CLI, library, progress, caching and JSON API.
- [`docs/INTERNALS.md`](docs/INTERNALS.md): architecture, parser, WAF math,
  transport, caching and benchmarks.
- [`ROADMAP.md`](ROADMAP.md): practical next steps and explicit non-goals.

## License

MIT. See [`LICENSE`](LICENSE).
