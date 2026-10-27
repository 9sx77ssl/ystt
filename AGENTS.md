# AGENTS.md

This file is the working contract for humans and coding agents in this
repository. Update it whenever architecture, deployment or safety rules change.

## Mission

`ystt` is a typed, fast, resilient TikTok downloader for **public** posts. The
workspace ships:

- `ystt` — public Rust facade;
- `ystt-core` — resolver, parser, media pipeline, caches and progress;
- `ystt-net` — `wreq` HTTP client, browser emulation and host limits;
- `ystt-waf` — challenge parser and parallel SHA-256 solver;
- `ystt-cli` — small CLI;
- `ystt-api` — authenticated Axum HTTP API with generated OpenAPI 3.1.

The Telegram bot lives in the separate private repository `ystg` and consumes
the public `ystt` library.

## Non-negotiable constraints

- Public content only. Never add account cookies, login-only posts, mobile
  private APIs, proxies or credential scraping.
- Keep the browser TLS/JA3/JA4 and HTTP/2 emulation intact unless a measured
  transport change is explicitly requested.
- No CI/GitHub Actions, Redis/S3 or external hosted cache is required.
- API and bot media must stay in RAM. Do not add disk-backed media caches to
  those paths. The CLI file cache is intentionally CLI-only.
- Never commit tokens, API keys, cookies, private keys or `.env` files.
- Do not log authorization headers, API keys or full user media.
- Keep public documentation in English. Preserve typed, modular, OOP-style
  design and avoid duplicated download logic.
- Every change must pass the verification checklist below.

## Workspace map

```text
crates/ystt-waf/    challenge parsing + parallel solver
crates/ystt-net/    wreq client, ALPN Auto, retries, host limits
crates/ystt-core/   URL/model/parser/download pipeline/media/caches/progress
crates/ystt-cli/    clap front-end
crates/ystt-api/    Axum API, DTOs, OpenAPI, Bearer auth, memory store
deploy/             API service, nginx snippets, API env example
docs/               GUIDE, API, INTERNALS
examples/           small examples, including in-memory downloads
tests/              offline tests + local HTTP integration harness
```

Important core modules:

- `download/options.rs` — resolved config/override merge;
- `download/job.rs` — disk download state machine;
- `download/bytes.rs` — generic in-memory video/gallery/audio path;
- `media/transfer.rs` — single stream, resume, adaptive ranges, in-memory
  ranges;
- `api/src/downloader.rs` — API result cache and media descriptors;
- `api/src/store.rs` — bounded RAM media store;
- `api/src/openapi.rs` — generated schema.

## Commands

From the repository root:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo test --features integration-tests --test integration_pipeline
cargo doc --workspace --no-deps
cargo build --release -p ystt-cli
cargo build --release -p ystt-api
```

The integration harness uses plain HTTP on loopback and therefore requires the
`integration-tests` feature. Production builds enforce HTTPS in `ystt-net`.

## Current transport and performance invariants

- `TransportPolicy::Auto` is the default: HTTP/2 via ALPN, HTTP/1.1 fallback for
  H1-only CDN hosts.
- Browser emulation defaults to Chrome 149.
- One shared HTTP client/connection pool per `Ystt`.
- `Http2Tuning` may change connection window/keep-alive only; never alter the
  browser fingerprint.
- Disk downloads use adaptive range planning; large in-memory downloads use
  parallel HTTP ranges assembled in RAM.
- API defaults: 128 MiB media cap, 3 workers, 2 batch concurrency, 8 MiB range
  threshold, 6 range connections.
- The Telegram bot uses `supports_streaming(true)` and real TikTok cover URLs.
- Never add throughput metrics to public/API responses unless explicitly
  requested; stage timings are allowed.

## HTTP API invariants

- Public base: `https://ystt.d9v.uk/api`; local bind `127.0.0.1:8091`.
- Every `/v1` route, including `/v1/openapi.json`, requires
  `Authorization: Bearer <key>`.
- API key comes from `YSTT_API_KEY`; the production file is
  `/etc/ystt-api/api.env` (`0600`). Never print or commit it.
- Auth comparison is constant-time. Keep it that way.
- Keep rate limiting at both Nginx (`CF-Connecting-IP`) and app level.
- Keep request bodies small; current Nginx/API limit is 64 KiB.
- Media tokens are opaque 128-bit random values and expire. They are not
  durable object IDs.
- OpenAPI is generated from Rust DTOs. Add new request/response types to
  `ApiDoc` and document status codes.
- Result cache is bounded and keyed by canonical post + complete options. A hit
  must be returned only while every referenced media token is still live.
- Do not add an arbitrary URL fetch endpoint. TikTok URL validation is the SSRF
  boundary.

Relevant production files:

```text
/etc/ystt-api/api.env
/opt/ystt-api/ystt-api
/etc/systemd/system/ystt-api.service
/etc/nginx/sites-available/ystt.d9v.uk
```

## Telegram bot invariants

The bot is a separate repository, but it must remain compatible with the
current `ystt` release:

- `teloxide-fork 0.18.0` under the `teloxide` alias;
- webhook mode only, loopback bind;
- in-memory downloads only;
- real cover + `supports_streaming(true)`;
- channel mirror `-1004292297389` with reusable `file_id`;
- local Bot API server on `127.0.0.1:8084`, `--local`, tmpfs work/temp;
- channel, URL and media caches are bounded and in RAM;
- no media files and no account cookies.

See `../ystg/AGENTS.md` for the bot-specific contract.

## Adding a feature

1. Put protocol/download behavior in `ystt-core`; never fork pipeline logic in
   the API or bot.
2. Add typed options/results to core if they are general.
3. Wire the CLI flag in `ystt-cli` and the API field/DTO in `ystt-api`.
4. Add the type/path to `ApiDoc`; verify OpenAPI.
5. Add tests at the lowest stable layer and at least one integration/boundary
   test when the behavior crosses HTTP.
6. Update `README.md`, `docs/GUIDE.md`, `docs/API.md`, `docs/INTERNALS.md`,
   `ROADMAP.md` and this file as applicable.
7. Deploy only after `fmt`, `clippy`, workspace tests, integration tests and a
   real endpoint smoke test.

This is explicit wiring, not code generation. The core and DTO schemas are
shared, but a new CLI flag or HTTP route is never assumed to appear by magic.

## Verification checklist

- `cargo fmt --all --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`
- `cargo test --features integration-tests --test integration_pipeline`
- `RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps` (known Cargo
  `ystt` lib/bin output collision warning is acceptable)
- API: no key -> 401, valid key -> 200, invalid JSON -> 400, oversized body ->
  413, media Range -> 206/416, OpenAPI security scheme present.
- Nginx: `nginx -t`, TLS 1.3, HTTP/2, rate limit.
- Deploy with atomic binary replacement; never overwrite a running executable
  in place.

## Deployment

- API and bot listen only on loopback; nginx terminates public TLS.
- Cloudflare fronts `ystt.d9v.uk`; edge TLS 1.3/HTTP/3 is provided by
  Cloudflare.
- Nginx origin hardening: TLS 1.2/1.3, modern ciphers, HSTS/security headers,
  upstream keepalive, request limits and per-client rate limiting.
- Secrets live only in root-owned environment files, never in git.
- API and bot are independent systemd services: `ystt-api.service`,
  `ystg.service`.
