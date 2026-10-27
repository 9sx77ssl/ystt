# ystt

**Yet Simple TikTok Toolkit** is a focused Rust library, CLI, Telegram bot and
authenticated HTTP API for downloading public TikTok posts **and public
SoundCloud tracks/playlists** with as little protocol overhead as possible.

The project is deliberately small: one typed provider layer, no browser
automation, no account cookies, no proxies, no mobile private API and no
ffmpeg/yt-dlp fallback. TikTok links use the rehydration/WAF pipeline; SoundCloud
links use the public player API, dynamic `client_id` discovery, and native
fMP4/MP3 HLS assembly. Everything is in memory for the API and bot.

## Features

### TikTok

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
- In-memory video, gallery and audio downloads for bots, HTTP APIs and streaming
  services, with no media files written to disk.
- TikTok cover URLs in the public media model for real video previews.
- Parallel HTTP range downloads assembled directly in memory for large videos.
- Bounded in-memory media store plus URL/options → response cache for the API.
- Optional shared cache root across output directories.
- Cross-process file-cache locks.
- Resumable videos and configurable filename templates.
- Optional HTTP/2 connection/keep-alive tuning without changing the TLS/JA3/JA4
  or HTTP/2 settings fingerprint.
- Expiry-aware signed-URL cache.
- Unified typed progress for solo downloads and batches.
- Stage timings.
- Partial gallery results with per-item retry control.
- JSON-ready results, stable `ErrorInfo` payloads, generated OpenAPI 3.1 and
  Bearer-authenticated HTTP endpoints.
- Nginx/Cloudflare edge hardening, app-level/Nginx rate limits and bounded
  request bodies for the API.
- No account cookies; only a filtered set of server session cookies is kept for
  the media CDN and, when the metadata cache is enabled, persisted with 0600
  permissions for the short signed-URL lifetime.

### SoundCloud

- Public tracks and `/sets/` playlists.
- Dynamic public `client_id` discovery and caching; no cookies, login, Go+ or
  browser emulation at runtime.
- Full playlist hydration: SoundCloud returns ID-only stubs for most large-set
  tracks; the client hydrates them in batches and preserves playlist order.
- Native audio transfer with **no ffmpeg and no yt-dlp**:
  - AAC 160/96 kbps fragmented-MP4 HLS (`aac_160k`, `aac_96k`);
  - MP3 HLS and progressive MP3;
  - Opus/HLS fallback;
  - `EXT-X-MAP` init segment + ordered parallel segment assembly.
- Quality policy mapping: `best`/`balanced` prefer 160 kbps AAC, `fast` prefers
  96 kbps, `compatible` prefers MP3.
- Track/playlist metadata, real artwork, duration, genre, policy checks.
- Downloaded 300×300 JPEG artwork sidecars and authenticated API artwork
  descriptors; Telegram gets the real square cover.
- Optional disk cache for SoundCloud audio (track ID + quality, 24 h TTL,
  content-addressed objects, atomic manifests). CLI-only; API/bot stay RAM-only.
- Native fallback across AAC/MP3/Opus candidates and one signed-URL refresh;
  playlist hydration falls back from batch IDs to per-track lookups.
- `BLOCK`/`SNIP`/encrypted-HLS tracks are rejected with a typed error instead of
  producing a broken preview file.
- Auto-detection in CLI/API/bot, including mixed TikTok + SoundCloud batches.
- Playlist support in CLI, HTTP API (`playlist` descriptor), Telegram messages
  and inline results.

## HTTP API

The workspace also ships `ystt-api`, a typed authenticated Axum API over the
same in-memory pipeline. It generates OpenAPI 3.1 from the Rust DTOs, requires a
Bearer key on every route, supports video/gallery/audio downloads, batch work,
range media streaming and bounded in-memory media tokens.

- Production base: `https://ystt.d9v.uk/api`
- Auth: `Authorization: Bearer <YSTT_API_KEY>` on every `/v1` route
- Schema: `GET /api/v1/openapi.json` with the Bearer key
- Endpoints: health, resolve, download, batch, media (Range), stats, OpenAPI
- Media is returned as short-lived authenticated tokens; bytes live in RAM
- TikTok and SoundCloud auto-detection; SoundCloud playlists return a typed
  `playlist` descriptor with per-track audio tokens
- API guide: [`docs/API.md`](docs/API.md)
- Crate: [`crates/ystt-api`](crates/ystt-api)

## Requirements

- Rust 1.99+ (edition 2024).
- Linux, macOS or Windows.
- A C toolchain, CMake, Perl and Clang for BoringSSL.
- No ffmpeg, yt-dlp, browser runtime, SoundCloud account or API key is required.

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
ystt --filename "{artist}-{title}-{quality}" URL
ystt --resume URL    # continue a TikTok video from a deterministic .part file
```

Examples:

```bash
ystt "https://www.tiktok.com/@user/video/123" -o downloads
ystt --audio "https://www.tiktok.com/@user/video/123" -o downloads
ystt "https://www.tiktok.com/@user/photo/456" -o downloads
ystt --info --json "https://vm.tiktok.com/XXXX/"
ystt "https://soundcloud.com/user/track" -o music
ystt "https://soundcloud.com/user/sets/playlist" -o music
ystt --info --json "https://soundcloud.com/user/track"
ystt --urls-file mixed.txt --concurrency 8 -o downloads --json
```

Defaults: quality `balanced`, file and metadata caches enabled, 12 photo workers,
adaptive ranges for files over 64 MiB. `--no-cache` disables both caches;
`--cache-dir` shares the file and metadata caches across output directories.
With `--json`, failures are emitted as a structured `ErrorInfo` object on stderr.
`--progress` emits the unified `ProgressEvent` stream as JSON lines on stderr.
For SoundCloud, `--filename` also supports `{artist}`, `{title}`, `{post_id}`
(track ID) and `{quality}`; playlist files are written in track order. A mixed
URL file preserves input order while downloading with bounded concurrency.
SoundCloud downloads also write `{name}.jpg` next to the audio. The default
SoundCloud disk cache lives in `<output>/.ystt-cache/soundcloud` (or
`<cache-dir>/soundcloud`) and removes the audio transfer on repeat runs;
`--no-cache` disables it. The API and bot never use this disk cache.

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

### SoundCloud

```rust
use ystt::{DownloadOptions, SoundCloudEntity, Ystt};

let ystt = Ystt::builder().file_cache(false).metadata_cache(false).build()?;

match ystt.resolve_soundcloud(url).await? {
    SoundCloudEntity::Track(track) => {
        let audio = ystt
            .download_soundcloud_resolved_track(&track, &DownloadOptions::new())
            .await?;
        println!("{} / {} ({}, {} bytes)", audio.track.artist, audio.track.title, audio.mime, audio.bytes.len());
    }
    SoundCloudEntity::Playlist(playlist) => {
        println!("{} tracks", playlist.tracks.len());
    }
}
```

`download_soundcloud_track` / `download_soundcloud_track_with` resolve and
download one URL. `download_soundcloud_playlist_with` and
`download_soundcloud_resolved_playlist` download a whole set with bounded
concurrency and per-track failures. `SoundCloudAudio` exposes bytes, MIME,
extension, selected preset, artwork, title, artist and duration. No media is
written to disk by these methods.

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
the complete video as `bytes::Bytes`. Videos above the configured range threshold
use parallel CDN range requests and are assembled in RAM. They never create media
or cache files; metadata caching can be enabled independently. `InMemoryVideo`
also exposes the post ID, author, dimensions, duration, cover URL and stage
timings.

For galleries and audio use the generic in-memory API:

```rust
let post = ystt
    .download_bytes_with(
        url,
        DownloadOptions::new().include_audio(true).max_bytes(128 * 1024 * 1024),
    )
    .await?;
println!("{} photos, audio={}", post.photos.len(), post.audio.is_some());
```

`InMemoryPost` contains video, photo and audio `InMemoryFile` values, failures and
timings. No output directory is involved.

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

Measured on x86_64 Linux, release build, no proxy and no account cookies. The
SoundCloud comparison uses the same Bun fetch/HLS algorithm as
[`bench/soundcloud_bun.ts`](bench/soundcloud_bun.ts), not the old TypeScript
module's yt-dlp fallback. Both implementations produce the same SHA-256 output.

| Workload | ystt | Bun reference |
|---|---:|---:|
| 30.35 MiB video, cold | ~2.8–3.3 s | ~2.6–4.2 s |
| 30.35 MiB video, warm file cache | **0 ms** | ~2.8 s |
| 23-photo gallery, 12 workers | ~0.82–1.25 s | ~1.00–1.55 s |
| small video, cold | 774.8 ms | 824.5 ms |
| SoundCloud 4.17 MiB AAC track, cold (7 runs) | **1.61 s median** | 1.64 s median |
| SoundCloud 4.17 MiB AAC track, peak RSS | **22.5 MiB** | 45.7 MiB |
| SoundCloud 4.17 MiB AAC track, CPU | **0.057 s** | 0.125 s |
| SoundCloud 15-track playlist, cold | **5.0 s** | n/a (TS reference rejects playlists) |
| SoundCloud 184-track playlist, API cold | **24.3 s** | n/a |

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
cargo test -p ystt-api
cargo build --release -p ystt-api
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
crates/ystt-api/    Axum API, DTOs, OpenAPI, Bearer auth, RAM store
deploy/             API service, nginx snippets and environment example
docs/               user guide, API and engineering internals
examples/           download and benchmark examples
tests/              offline and local-HTTP integration tests
```

## Documentation

- [`docs/GUIDE.md`](docs/GUIDE.md): CLI, library, progress, caching and JSON API.
- [`docs/API.md`](docs/API.md): authenticated HTTP API, OpenAPI, limits and edge.
- [`docs/INTERNALS.md`](docs/INTERNALS.md): architecture, parser, WAF math,
  transport, caching, API layer and benchmarks.
- [`AGENTS.md`](AGENTS.md): coding-agent contract, invariants and deployment.
- [`ROADMAP.md`](ROADMAP.md): practical next steps and explicit non-goals.

## License

MIT. See [`LICENSE`](LICENSE).
