# SoundCloud provider — working plan

## Goal

Add SoundCloud as a first-class provider next to TikTok across:
- `ystt-core` library and `ystt` facade;
- CLI;
- authenticated HTTP API + OpenAPI;
- Telegram bot/inline/channel;
- tests, docs, deployment and benchmarks.

Only public tracks/playlists. No account cookies, no login/Go+, no ffmpeg/yt-dlp
fallback, no browser automation at runtime.

## Phase 1 — Provider and client

- [x] Add `Provider` enum without breaking TikTok.
- [x] Add `soundcloud` core module.
- [x] Discover/cache public SoundCloud `client_id`.
- [x] Resolve `soundcloud.com` tracks, short links and `/sets/` playlists.
- [x] Parse artwork, title, artist, duration, policy and transcodings.
- [x] Reject block/SNIP/no-stream tracks.
- [x] Cap/guard playlist size and malformed API responses.

## Phase 2 — Audio transfer

- [x] Choose best pure-Rust compatible transcoding:
  - [x] AAC fMP4 HLS (`aac_160k`/`aac_96k`);
  - [x] MP3 HLS/progressive;
  - [x] Opus as fallback.
- [x] Fetch SoundCloud stream endpoint and final CDN URL.
- [x] Parse HLS master/media playlists.
- [x] Handle `EXT-X-MAP` init segment.
- [x] Parallel segment download, ordered in-memory assembly.
- [x] Reject encrypted HLS rather than silently writing broken audio.
- [x] Validate output and keep no disk writes.
- [x] Optional artwork descriptor (URL); API/bot expose it.

## Phase 3 — Provider-neutral API/CLI/bot

- [x] Auto-detect TikTok vs SoundCloud by host.
- [x] Extend CLI: track, playlist, metadata, JSON, quality, artwork.
- [x] Extend API `/v1/download` and `/v1/resolve`; add OpenAPI schemas.
- [x] Extend bot: audio placeholders, sendAudio, inline audio, channel mirror,
      file_id/artwork cache.
- [x] Mixed TikTok + SoundCloud batches preserve order.
- [x] Keep existing TikTok flags/semantics working.
- [x] Provider namespaced cache keys.

## Phase 4 — Tests and benchmarks

- [x] Unit tests: client-id parser, URL detection, HLS parser, transcode
      selection, artwork normalization, policy rejection.
- [x] Local fixture tests: HLS init + segments and malformed input.
- [x] Integration tests for track, playlist, mixed batch (live + API).
- [x] Real smoke tests with supplied public links.
- [x] API auth/rate/OpenAPI/media tests.
- [ ] Bot smoke (requires deployed Telegram token; local tests cover handlers).
- [x] Bun TS baseline vs Rust:
  - [x] track cold/warm;
  - [x] artifact size/hash;
  - [x] CPU/RSS;
  - [x] HLS segment throughput;
  - [x] playlist throughput (Rust; TS module rejects playlists).
- [x] Record wins/losses honestly.

## Phase 5 — Docs/deploy

- [x] `README.md`, `docs/GUIDE.md`, `docs/API.md`,
      `docs/INTERNALS.md`, `ROADMAP.md`, `AGENTS.md`.
- [ ] Deploy API/bot only after full local tests.
- [ ] Repo description/topics update for TikTok + SoundCloud.
- [x] Final security review: SSRF, host allowlist, auth, body limits, caches.
