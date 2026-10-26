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
- Expiry-aware signed-URL cache.
- Typed progress events and stage timings.
- Partial gallery results with per-item retry control.
- JSON-ready results and stable `ErrorInfo` payloads for HTTP APIs.
- No account cookies; only server-set session cookies are kept in memory.

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
           [--audio] [--no-cache]
```

Examples:

```bash
ystt "https://www.tiktok.com/@user/video/123" -o downloads
ystt --audio "https://www.tiktok.com/@user/video/123" -o downloads
ystt "https://www.tiktok.com/@user/photo/456" -o downloads
ystt --info --json "https://vm.tiktok.com/XXXX/"
```

Defaults: quality `balanced`, file cache enabled, 12 photo workers, adaptive
ranges for files over 64 MiB.

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

`Ystt` is cloneable and safe to share between async tasks. `MediaInfo`,
`DownloadResult`, `Timings`, `DownloadedFile` and `ErrorInfo` implement
`serde::Serialize`.

## Transport

The default `TransportPolicy::Auto` lets ALPN choose per host:

- TikTok pages and the `v16`/`v19` video CDNs use HTTP/2.
- The `v45` music CDN and other H1-only hosts use HTTP/1.1.
- There is no manual host list and no failed HTTP/2 handshake before fallback.

`Http2Only` and `Http1Only` are available as explicit advanced overrides.
Production builds require HTTPS. Server session cookies are kept in memory only
and are never persisted.

## Caching

```text
<output>/.ystt-cache/
├── manifests/<post_id>_<quality>_<audio>.json
└── objects/<blake3>.<ext>
```

- First download stores immutable objects and writes an atomic manifest.
- A cache hit copies the object into the output directory without HTTP.
- Entries expire after `file_cache_ttl` (default 24 h).
- Expired, malformed or missing-object manifests are removed on read.
- `Ystt::prune_file_cache()` removes stale manifests and unreferenced objects.
- `Ystt::clear_file_cache()` removes everything.
- `Ystt::invalidate_file_cache(...)` and `invalidate_url_cache(...)` drop one entry.
- `--no-cache` disables the file cache for one call.

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
and covers video ranges, session cookies, music, progress events, file-cache
hits, partial galleries and the embed fallback.

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

## License

MIT. See [`LICENSE`](LICENSE).
