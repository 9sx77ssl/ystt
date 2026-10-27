# ystt roadmap

This file lists only work with a concrete user or maintenance benefit. No CI,
analytics, hosted API, Redis/S3 cache, or web framework is part of the project.

## Current baseline

- public TikTok videos, photo galleries and music;
- full-page and embed resolvers;
- ALPN Auto transport: HTTP/2 preferred, HTTP/1.1 fallback;
- parallel WAF solver;
- one-request media path for typical videos, adaptive ranges for large files;
- content-addressed file cache with cross-process locks;
- persistent metadata/signed-URL cache;
- batch downloads with ordered results, progress and per-item cancellation;
- resumable single-stream video downloads;
- filename templates;
- typed progress and JSON-ready results.

## Completed internal refactors

- `ResolvedOptions` merges `YsttConfig` and per-call overrides once.
- `DownloadJob` owns the cache -> resolve -> transfer -> store-cache state
  machine.
- `media.rs` is split into `media/transfer.rs`, `media/validation.rs` and
  `media/ext.rs`.
- 403 re-resolution, cache hit, partial gallery, cancellation, resume, audio
  and filename-template paths have integration coverage.

## Next: internal maintainability

1. **Consistent error context**
   - add a structured error chain for media failures (host, kind, status,
     attempt) without recording throughput metrics.

2. **Resolver split**
   - move page/embed/short-link resolution out of `download.rs`;
   - keep the public resolver API unchanged.

## Next: user-facing reliability

1. **Resume for adaptive ranges**
   - persist a small sidecar with total size, ETag and completed ranges;
   - reuse validated ranges after interruption;
   - keep the current single-stream resume as the fallback.

2. **Output templates**
   - add `{ext}`, `{kind}` and `{index}` placeholders;
   - collision policy: fail or append a counter;
   - keep all path components sanitized.

3. **Quality policy**
   - add an `efficient` policy preferring H.265 when it is both smaller and
     visually equivalent;
   - keep `balanced` as the compatibility default.

4. **Batch controls**
   - add per-item timeout and retry override;
   - add batch-level cancellation that waits for in-flight writes;
   - expose aggregate item counts in `BatchResult` (already emitted as
     `BatchProgress`).

5. **CLI**
   - accept a TOML/JSON config file;
   - add `--timeout`, `--retries`, `--browser` and `--quiet`;
   - keep `--urls-file`, `--progress`, `--filename` and `--resume` stable.

## Later, only with measurements

1. **HTTP/2 window and keep-alive experiments**
   - benchmark per CDN before changing defaults;
   - never replace the browser TLS/JA3/JA4 fingerprint.

2. **Protocol/connection prewarming**
   - long-lived process only;
   - measure cold and warm paths separately.

3. **Range planning**
   - tune first chunk and concurrency per file size;
   - compare against one-stream path on the same edge.

## Explicitly out of scope

- web API or hosted service;
- Axum/OpenAPI/SSE/WebSocket server;
- Redis/S3/remote cache;
- account cookies or login-only content;
- mobile private API and request signatures;
- HTTP/3;
- CI and GitHub Actions.
