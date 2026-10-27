# ystt — internals

This is the engineering document: architecture, protocol details, math, pipeline,
performance and decisions. The user-facing API is in `GUIDE.md`.

## 1. Workspace

```text
ystt-waf   (no ystt deps)   challenge parsing + parallel SHA-256 solver
ystt-net   (no ystt deps)   wreq client, emulation, retries, rate limits
ystt-core  (net + waf)      URL/model/parser/media/caches/progress/pipeline
  download/{types,options,job,bytes,batch,builder};
  media/{transfer,validation,ext}. Transport, cache and cookie helpers are
  independent modules. The API depends only on the public core facade.
ystt-cli   (core)           minimal clap front-end
ystt-api   (core)           Axum HTTP API, DTOs, OpenAPI, RAM store
ystt       (root)           public facade + offline/integration tests
```

Dependency direction is one-way. `ystt-waf` has no HTTP knowledge;
`ystt-net` has no TikTok knowledge.

## 2. URL and page parsing

### Supported URL forms

- `/@user/video/<id>`, `/@user/photo/<id>`, `/@i/video/<id>`;
- `/t/<code>`, `vm.tiktok.com/<code>`, `vt.tiktok.com/<code>`;
- `?item_id=<id>` / `?share_item_id=<id>`;
- bare 17+ digit post ID inside a TikTok URL.

The parser is manual: no regex, no static `expect`, no panics. Short links are
resolved with one GET and `redirect: manual`:

1. `Location`;
2. canonical link;
3. `og:url`;
4. first anchor;
5. `"itemId"`.

### The `/video/` rule

TikTok serves the rehydration payload for **both videos and photo posts** on
`/video/<id>`. `/photo/<id>` contains the marker but no `webapp.video-detail` or
`itemStruct`. `TikTokUrl::page_url` therefore always builds `/video/<id>`.

### Rehydration JSON

`__UNIVERSAL_DATA_FOR_REHYDRATION__` → `__DEFAULT_SCOPE__` →
`webapp.video-detail` → `itemInfo.itemStruct`.

URL fields can be a string, a list, or `{ urlList | UrlList | url_list }`.
Only absolute HTTPS URLs with a host are accepted. Photo posts can contain a
relative placeholder `video` object; when gallery photos exist, video is ignored.

### Official embed fallback (videos)

For video URLs, `ystt` normally fetches the full page because its `playAddr`
H.264 stream is the smallest/fastest tested file. If the full page fails
(network, parse, WAF failure), it tries the official embed page:

```text
https://www.tiktok.com/embed/v2/<id>
```

The embed page contains `__FRONTITY_CONNECT_STATE__` →
`source.data.*.videoData.itemInfos` with `video.urls[]` and
`video.videoMeta.width/height/duration`. It has no gallery images, so photo
posts never use it. Its media URL does not require `tt_chain_token`, which makes
it a useful fallback when the full page is challenged.

`use_embed(true)` changes the order to embed-first. Full-page and embed requests
use the same ALPN Auto client: HTTP/2 where negotiated, HTTP/1.1 for H1-only
hosts.

### Quality selection

- `best`: max bitrate;
- `compatible`: max H.264 bitrate;
- `balanced`: highest H.264 within 1920 px (default; smallest compatible file);
- `fast`: min bitrate at least 360p.

`MediaInfo` exposes bitrates, codec, width, height and duration. `--info` prints
all of them.

## 3. WAF challenge

### Format

```html
<p id="cs" class="<base64 challenge>"></p>
<p id="wci" class="<cookie name>"></p>
```

JSON:

```json
{ "v": { "a": "<base64 prefix>", "c": "<base64 or hex sha256>" }, "d": "" }
```

Find `n` such that:

```text
SHA256(prefix || decimal(n)) == digest
```

Set `d = base64(decimal(n))`, serialize the entire JSON (preserving unknown
fields), base64-encode it, and send `name=value` as a cookie.

### Parser

- tolerant of attribute order, quotes, whitespace and uppercase;
- standard, no-pad and URL-safe base64;
- digest accepts 32-byte base64 or 64-char hex;
- cookie name rejects `;`, `=`, `,`, control characters;
- fingerprint = SHA-256(cookie_name || 0 || prefix || digest), used by the
  in-process solved-cookie cache;
- external CAPTCHA pages (Cloudflare, “verify you are human”) are detected and
  returned as a typed parse error instead of looping.

### Math

Uniform nonce in `[0, N-1]`, `N = max_iterations + 1`.

Single thread:

```text
E[hashes] = (N - 1) / 2
worst     = N
```

`T` threads, chunk `C = ceil(N / T)`:

```text
E[wall hashes] = (C - 1) / 2 ≈ N / (2T)
worst          = C
```

If every thread hashes `k` candidates:

```text
P(found)     = min(1, kT / N)
P(not found) = max(0, 1 - kT / N)
```

The solver uses contiguous chunks, cloned SHA-256 states, `itoa`, an
`AtomicBool` stop flag checked every 1024 candidates, and an `AtomicU64` result.
It runs on `spawn_blocking`, never on an async worker. The iteration cap is a
hard safety bound (default 2,000,000).

## 4. HTTP and anti-bot

- `wreq` + `wreq-util`; default Chrome 149 on Windows;
- BoringSSL, header-case preservation, HTTP/2 settings and ALPN emulation;
- transport policy defaults to **Auto**: ALPN prefers HTTP/2 and automatically
  uses HTTP/1.1 for H1-only CDNs; HTTPS is required in production;
- page and video traffic normally negotiate H2; `v45` music negotiates H1.1;
- page headers add navigation `sec-fetch-*`;
- media headers add video/image/audio `sec-fetch-*`, `referer`, `range`, cookies,
  and `accept-encoding: identity` (media is already compressed);
- one shared `wreq::Client` for the lifetime of `Ystt`; page and embed share its
  connection pool and TLS session cache;
- retries only for GET/HEAD; statuses 408, 425, 429, 500, 502, 503, 504;
- exponential backoff + jitter, `Retry-After` capped by max delay;
- per-host page limit: 2 concurrent, 120 ms spacing; media is not rate-limited;
- host limiter state is capped at 256 hosts;
- timeouts: 20 s request, 10 s connect, 60 s body idle, 90 s pool idle;
- at most 8 idle pooled connections per host;
- `no_proxy` is enabled by default.

`TransportPolicy::Auto` is the production choice. The client does not force a
protocol: ALPN uses H2 where available and H1.1 otherwise. This is what lets the
same page/video pipeline use H2 while `v45` music uses H1.1. `Http2Only` and
`Http1Only` are advanced/debug overrides, not defaults.

The feature-gated local integration server is HTTP/1.1 only; production
`https_only(true)` is enabled outside that feature.

### HTTP/2 tuning

`wreq` browser emulation supplies a complete HTTP/2 profile: SETTINGS order,
pseudo-header order, stream dependency, header table, windows and frame size.
`Http2Tuning` is applied by cloning the emulation and modifying only
connection-level window and keep-alive fields. TLS/JA3/JA4 and every HTTP/2
fingerprint field remain untouched. Defaults are unchanged; the tuning is
opt-in for deployments that need larger long-lived connections.

The client shares one `wreq::Client`; page, video, photo and audio traffic reuse
its pool and TLS session cache. The negotiated HTTP version is included in debug
logs.

Server session cookies (`ttwid`, `tt_chain_token`, `tt_csrf_token`) are captured
from the page response and sent to media. In memory they live in a `CookieJar`.
When the metadata cache is enabled, only those three names are persisted in a
0600 JSON entry for the signed-URL lifetime. A media `403` invalidates the
signed-URL cache, re-resolves the page once, and retries.

## 5. Download pipeline

```text
parse URL
  ├─ direct ID
  └─ short link → redirect/HTML
file-cache lookup (post_id, quality, audio)
  └─ hit → copy objects to output → done
in-memory signed-URL lookup
  └─ hit → skip page
persistent metadata lookup (post_id, quality)
  └─ hit → load MediaInfo + filtered session cookie → skip page
page GET → parse rehydration
  └─ no item → WAF challenge → solve → retry with cookie
quality policy → MediaInfo
persist metadata + filtered session cookie
media plan
  ├─ photo gallery: concurrent single streams
  ├─ video: range-first adaptive
  └─ audio: single stream
validate signatures
atomic rename
store file cache (if complete)
```

### Option resolution and job state

`ResolvedOptions` merges `YsttConfig` and `DownloadOptions` once per job. The
pipeline never reads raw config fields or repeated `unwrap_or` defaults.

`DownloadJob` owns the per-URL state machine:

```text
file-cache lookup / lock
  -> resolve (URL cache, metadata cache, page/embed)
  -> transfer (video, photos, audio)
  -> 403 re-resolve (once)
  -> partial policy
  -> file-cache store
  -> timings + progress
```

`download_with` is a thin wrapper that creates the progress context and maps
errors to `ProgressStage::Failed`. `download_many` reuses the same job for every
item with batch context and per-item cancellation.

### Batch and progress

`Ystt::download_many` runs independent URL jobs with `buffer_unordered`, indexed
results and a bounded concurrency. It reuses the same HTTP pool, WAF solver and
caches. Results are sorted back into input order; failures are captured as
`ErrorInfo` slots instead of aborting the batch.

Every single download, resolve call and batch emits context-aware
`ProgressEvent`s. Each event includes `job_id`, optional `url`, and for batch
items `batch_index` / `batch_total`. `BatchStarted` and `BatchCompleted` frame
the batch; item failures emit `Failed`; `BatchProgress` carries aggregate
completed/failed counts. Media progress is reported by the same
`ProgressTracker` used by solo downloads. `BatchOptions::progress` attaches a
reporter to the batch and all its items.

### Adaptive video transfer

The first request is a **normal browser-like GET**, not a Range request. This is
important: TikTok's CDN serves small files faster with `200` than with `206`.

1. Normal GET.
2. Small file (`Content-Length <= range_threshold`, default 64 MiB), unknown
   size, or no `Accept-Ranges: bytes` → stream the body directly.
3. Large file → write the first `first_chunk` bytes from the running `200` body,
   then request the remaining ranges with `Range` + `If-Range`.
4. Any range failure aborts the rest and falls back to a single stream.

Follow-up ranges write at their offsets. `AdaptiveRangeController` starts at
`range_start_connections` (default 2), ramps up when throughput improves by 5%,
backs off on a 30% drop, and is clamped to `1..=max_connections-1` follow-ups.
`max_connections` includes the first chunk.

Small files (≤ `range_threshold`) use one request and one connection. For a
2.67 MiB video the whole transfer is a single `200` body, matching the browser.
The default 64 MiB threshold keeps typical TikTok videos on this faster path;
ranges remain available for very large files and are forced by tests.

### Buffered writes

`write_stream` is generic over `AsyncWrite`; callers wrap files in a 256 KiB
`BufWriter` (128 KiB in range tasks) to reduce syscalls.

### Validation

| Kind | Accepted |
|---|---|
| video | MP4 `ftyp`, WebM, RIFF AVI |
| photo | JPEG, PNG, WebP, GIF |
| audio | MP3/ID3, MP4/M4A, Ogg, WAV |

HTML pages returned with `200` are rejected. Size is capped by `max_bytes`
(default 512 MiB). Files are written to a hidden `.part` and renamed only after
validation.

## 6. File cache

```text
<output>/.ystt-cache/
├── manifests/<post_id>_<quality>_<audio>.json
└── objects/<blake3>.<ext>
```

- BLAKE3 hashes the finished file (spawn_blocking);
- objects are copied, not hardlinked, so editing an output cannot corrupt cache;
- manifests are written atomically (`.tmp` + rename);
- `locks/<key>.lock` serialises the whole job across processes; stale locks
  are reclaimed after 30 minutes;
- `get` verifies every object exists and has the recorded size;
- TTL is 24 h by default; expired manifests are removed;
- `prune` removes stale manifests and unreferenced objects;
- partial/failed galleries are not cached;
- cache hits materialise copies into the output directory and report
  `cache_hit = true`.

The signed-URL cache is split in two: an in-memory bounded cache for the current
process and an on-disk metadata cache for new processes.

### Metadata cache

```text
<user-cache>/ystt/metadata/<post_id>_<quality>.json
```

- schema version is explicit; old-schema entries are removed;
- expiry is derived from the signed media URLs and capped by
  `metadata_cache_ttl`;
- `MediaInfo` plus only `ttwid`, `tt_chain_token`, `tt_csrf_token` are stored;
- writes are atomic and 0600 on Unix;
- expired, malformed and missing-media entries are removed on read;
- capacity is bounded (256 entries, LRU-ish by file mtime);
- `cache_dir` shares both `files/` and `metadata/` between output directories.

## 7. Reliability rules

- no `unwrap`/`expect`/`panic` in library code;
- no `unsafe` (workspace lint `unsafe_code = "forbid"`);
- every temporary file is removed on error/cancellation;
- body reads are bounded and idle-timed;
- caches are bounded/TTL'd; metadata entries are schema-versioned and 0600;
- `JoinSet` aborts sibling ranges on failure;
- cancellation is checked before requests, during body reads and during sleeps;
- public types are documented (`missing_docs = "warn"`).

## 8. Testing

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test --features integration-tests --test integration_pipeline
```

`tests/` is offline. `integration_pipeline` uses a local `TcpListener`, serves
page JSON, session cookies, range responses, good and failing images, and verifies
the real pipeline, progress, file cache and partial results.

Manual real-network smoke (low volume):

```bash
./target/release/ystt --info "<video-url>"
./target/release/ystt --info "<gallery-url>"
./target/release/ystt "<video-url>" -o downloads
./target/release/ystt "<gallery-url>" -o downloads
```

Do not loop, fuzz or add account cookies/proxies.

## 9. Benchmarks

Environment: x86_64 Linux (Zen kernel), Rust 1.99.1, 2026-10-26, no proxy, no
account cookies, release build.

### Rust cold video (default full page, Balanced)

URL `@ddblunt4/video/7699969132195810568`, H.264 `playAddr`,
**1,141,666 bytes**. Ten fresh processes, `--no-cache`:

| Run | Total | Page | Media |
|---|---:|---:|---:|
| 1 | 859 ms | 625 ms | 233 ms |
| 2 | 775 ms | 565 ms | 209 ms |
| 3 | 799 ms | 566 ms | 231 ms |
| 4 | 845 ms | 634 ms | 210 ms |
| 5 | 778 ms | 596 ms | 182 ms |
| 6 | 741 ms | 574 ms | 166 ms |
| 7 | 746 ms | 592 ms | 153 ms |
| 8 | 689 ms | 534 ms | 154 ms |
| 9 | 773 ms | 593 ms | 179 ms |
| 10 | 743 ms | 567 ms | 175 ms |
| average | **774.8 ms** | **584.6 ms** | **189.2 ms** |
| median | **774.5 ms** | — | — |

A second 10-run sample on the same day: 935 / 768 / 912 / 938 / 774 / 801 /
778 / 926 / 827 / 848 ms — average **850.7 ms**, median **837.5 ms**. TikTok's
page TTFB moves with the CDN edge; the cache hit path is the deterministic win.

The earlier embed experiment was ~763 ms but downloaded 2,672,502 bytes. The
full-page Balanced path is faster per useful file and picks the much smaller
H.264 `playAddr`; `Best` still returns the 3,432,904-byte H.265 stream.

### Rust cold gallery

URL `@ilqqxz/photo/7674697655909600532`, 23 JPEG, 1,611,062 bytes.

| Concurrency | Total | Media | Photos |
|---:|---:|---:|---:|
| 4 | 1306 ms | 589 ms | 23 |
| **8** | **880 ms** | **283 ms** | 23 |
| 12 | 923 ms | 433 ms | 23 |
| 8 (another sample) | 1108–1180 ms | 493–648 ms | 23 |

Default is **8**. Page TTFB varies with the CDN edge, so gallery totals vary.

### Rust cache hit

Same gallery/video, second CLI process:

```text
total ≈ 0–1 ms, cache_hit = true, zero HTTP requests
```

### Single-GET threshold

URL `@traveling_show/video/7687283866586909984`, 30.35 MiB Balanced H.264,
measurements after CDN warm-up:

| Mode (full-page resolver) | Total | Page | Media |
|---|---:|---:|---:|
| normal `200` | 2.81–2.97 s | 0.59–0.66 s | 2.15–2.35 s |
| adaptive ranges | 2.88–3.30 s | 0.50–0.68 s | 2.34–2.76 s |

One stream was consistently slightly faster. `range_threshold` (default 64 MiB)
therefore keeps typical videos on the one-request path; ranges remain for very
large files and for tests that force them.

### Auto transport cold video vs Bun

Same 30.35 MiB video, five fresh processes each, no output cache:

| Runtime | Runs (ms) | Average | Median |
|---|---|---:|---:|
| Rust 1.1.0-1.2.0 | 3056 / 2773 / 2956 / 2939 / 2891 | **2923 ms** | **2939 ms** |
| Bun 1.4.3 | 2867 / 2786 / 2932 / 2884 / 2607 | **2815 ms** | **2867 ms** |

Cold single-file time is network-bound: page TTFB is ~0.55 s and the 30 MiB
media transfer is ~2.2–2.4 s on this edge. Rust is within noise of Bun cold.
The deterministic win is the cache: a second Rust process is **0 ms** with zero
HTTP requests, while Bun re-downloads every time. Gallery downloads are also
faster: 12 workers typically finish 23 JPEGs in **0.82–1.08 s** versus Bun
**1.00–1.55 s** on the same edge.

Protocol proof from `RUST_LOG=debug` on the same download:

```text
page fetched              version=HTTP/2.0
single media transfer     version=HTTP/1.1  bytes=3213314   # v45 music
adaptive single transfer  version=HTTP/2.0  bytes=31828479  # v16 video
```

Peak RSS for the cold Rust CLI download is **≈17 MiB** (page cap 4 MiB, 2 MiB
write buffer, bounded URL/WAF caches).

### Transport comparison

Same full-page resolver and single `200` media request, three cold runs per
policy (`bench_video full-single`, metadata and file cache disabled):

| Transport | Total | Page | Media |
|---|---:|---:|---:|
| Auto | 2.505–2.701 s | 0.436–0.516 s | 1.996–2.264 s |
| HTTP/1.1 | 2.539–2.781 s | 0.472–0.511 s | 2.039–2.308 s |
| HTTP/2-only | 2.778–3.063 s | 0.430–0.503 s | 2.315–2.632 s |

ALPN Auto remains the best default: it gets H2 where it helps and H1 where it is
required. Forcing H2 on this CDN was slower in every sample. The embedded
resolver showed the same pattern; HTTP/2-only also fails on `v45` media hosts.

### Batch CLI

Two real URLs (30.35 MiB video + 23-photo gallery), `--urls-file`,
`--concurrency 2`, parallel media:

```text
total ≈ 3.19 s, successes = 2, failures = 0
```

This is the main throughput win over a sequential client when multiple jobs are
available.

### Bun baseline (historical)

Same small video URL, ten cold runs with Bun 1.4.3:

| Runs | Average | Median |
|---|---:|---:|
| 1246.0 / 743.4 / 908.6 / 725.4 / 884.9 / 709.6 / 762.9 / 711.3 / 880.9 / 671.6 ms | **824.5 ms** | **753.2 ms** |

The Bun baseline downloaded the 3,432,904-byte stream; Rust Balanced downloaded
1,141,666 bytes at the same 576×576 H.264 resolution. Rust cold time is
essentially parity with Bun (one sample faster, one within noise), but Rust moves
**3× fewer bytes** and is thousands of times faster on a cache hit. The
TypeScript implementation is not part of this repository.

### WAF solver

10 fixed nonces, 1,000,001 candidates, release:

| Threads | Average |
|---:|---:|
| 1 | 34.38 ms |
| 2 | 17.81 ms |
| 4 | 8.50 ms |
| 8 | 4.55 ms |

≈14.5 M hashes/s per thread, near-linear scaling.

## 10. HTTP API

`ystt-api` is a separate library + binary in the workspace. It reuses the public
`ystt` pipeline and never shells out to the CLI.

### Surface

- Axum 0.8 on `127.0.0.1:8091`;
- public base `https://ystt.d9v.uk/api` behind nginx;
- Bearer auth on every `/v1` route, including the OpenAPI document;
- `/v1/health`, `/v1/resolve`, `/v1/download`, `/v1/batch`, `/v1/media/{token}`,
  `/v1/stats`, `/v1/openapi.json`;
- OpenAPI 3.1 generated from Rust DTOs with `utoipa`.

### In-memory pipeline

`Ystt::download_bytes_with` resolves one post and downloads video, gallery and
optional audio using `download_media_bytes`. Large video payloads use parallel
range requests and are assembled as `Bytes` in RAM. The transport reuses the
same HTTP/2 client, range planner and validation as disk downloads.

The API keeps two bounded structures:

1. `MediaStore` — bytes, MIME/extension, SHA-256, TTL and byte cap;
2. result cache — canonical post + complete options → `DownloadResponse`,
   bounded to 512 entries. A hit is valid only while every descriptor token is
   still present in the media store.

There is no Redis and no disk media cache. Result descriptors are short-lived
and authenticated; clients fetch bytes through `/v1/media/{token}`.

### Auth and abuse controls

- Bearer secret from `YSTT_API_KEY`, constant-time comparison;
- token-bucket rate limit per client;
- global and batch semaphores;
- 64 KiB request body limit;
- bounded batch size, media cap and in-memory store;
- nginx per-`CF-Connecting-IP` request/connection limits;
- nginx TLS 1.2/1.3, HSTS/security headers and access-log suppression;
- Cloudflare edge provides TLS 1.3 and HTTP/3 to clients.

### Deployment

```text
ystt-api.service            127.0.0.1:8091
/etc/ystt-api/api.env       0600 root, never in git
deploy/nginx-ystt.conf      canonical edge config
deploy/nginx-ystt-hardening.conf
deploy/nginx-ystt-api.conf
```

## 11. Decisions and limits

### Accepted

- `wreq` for browser fingerprinting, despite BoringSSL build cost;
- no account cookies; only filtered server session cookies for media;
- persistent metadata cache with filtered server session cookies, 0600 and TTL;
- batch API with bounded concurrency and ordered per-item results;
- shared cache root for file and metadata caches;
- opt-in HTTP/2 connection/keep-alive tuning without changing the fingerprint;
- manual URL/HTML parsing (no regex/panics);
- full page + H.264 `playAddr` as the default video path;
- official embed resolver as an opt-in/fallback for blocked video pages;
- normal GET for files up to 64 MiB, prefix + ranges only for larger files
  (no HEAD probe);
- ALPN Auto transport (H2 preferred, H1.1 fallback) with browser TLS/JA3/JA4
  and HTTP/2 fingerprint emulation; all known TikTok CDN hosts supported;
- BLAKE3 content-addressed file cache with copy-in/copy-out;
- adaptive range concurrency, default 12 photo workers;
- partial galleries enabled by default;
- no CI; local fmt/clippy/test/bench before every push;
- in-memory raw media path for API/bot, with bounded TTL stores;
- generated OpenAPI and Bearer-only HTTP API;
- no Redis: single-process RAM caches are sufficient for the current deployment.

### Evaluated and rejected

- **HTTP/3 / QUIC**: `wreq` advertises H3 ALPN but has no QUIC transport.
  A separate `quinn`/`h3` + rustls path would abandon the verified BoringSSL
  browser fingerprint, add a large dependency surface and may be blocked. No
  measured benefit for TikTok's current CDN path; not added.
- **Mobile API resolver**: requires fragile request signatures; out of scope.
- **Proxy layer**: infrastructure concern; explicitly out of scope.
- **Account cookie/session provider**: user asked for no own cookies.
- **Language bindings**: Rust library is sufficient for embedding.
- **Prometheus/OpenTelemetry**: progress events and timings cover the current
  needs without another dependency tree.
- **Redis as the primary media store**: would add network/persistence and
  contradict the in-memory API contract; reconsider only for horizontal scale.
