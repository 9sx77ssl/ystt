# SoundCloud benchmarks

Reproduce with:

```bash
cargo build --release -p ystt-cli
bun --version
./bench/compare.py
```

The Bun baseline is `bench/soundcloud_bun.ts`: the same public `client_id`
discovery, resolve call, HLS parsing and 8-way segment assembly as the Rust
client. It is intentionally **not** the old TypeScript module's `yt-dlp`
fallback, and it does not shell out to `ffmpeg`.

## Measured on this workstation

Environment: x86_64 Linux, 30 GiB RAM, 12 threads, Rust 1.99 release,
Bun 1.4.3. Seven cold runs for the track; three for the playlist. Both
implementations wrote the same 4,371,992-byte file with SHA-256
`a489fb2879bcd0b2d69915e93be89478f354d5ba1d84cf9bd63929cd18691f1c`.

| Workload | Rust `ystt` | Bun baseline |
| --- | ---: | ---: |
| 4.17 MiB AAC track, cold median | **1.608 s** | 1.639 s |
| Same track, peak RSS | **22.5 MiB** | 45.7 MiB |
| Same track, CPU user+sys | **0.057 s** | 0.125 s |
| 15-track playlist, cold | **5.02 s** | not supported by TS reference |
| 184-track playlist, API cold | **24.3 s** | not supported by TS reference |
| 184-track playlist, API artifact bytes | 680.2 MiB | n/a |
| 184-track playlist, API warm result cache | **0.19 s** | n/a |

## Honest wins/losses

- **Speed parity on one track.** The network/CDN dominates, so Rust and Bun are
  within measurement noise on elapsed time. Rust uses about half the CPU and
  half the peak RSS.
- **Functional win.** The reference TypeScript `soundcloud.ts` only selects
  MP3/Opus transcodings. Modern monetized tracks expose AAC-only HLS, so the TS
  native path throws and falls back to `yt-dlp`. The Rust module assembles the
  AAC fMP4 stream natively; the benchmark above succeeds without external tools.
- **Playlist win.** The TS reference explicitly rejects `/sets/`. Rust hydrates
  ID-only playlist stubs in batches, preserves order, and downloads 15 tracks in
  ~5.0 s locally.
- **Scaling warning.** The API's 184-track response holds ~680 MiB in the
  bounded media store. The default production store is 768 MiB; environments
  that routinely serve large playlists should raise `YSTT_API_STORE_MB` and use
  the CLI/bot streaming paths for very large sets. The CLI supports 184 tracks
  but writes one file at a time; the bot deliberately keeps only metadata while
  sending tracks sequentially to stay under its 128 MiB cap.
- **No external media stack.** Neither benchmark run uses ffmpeg, yt-dlp, a
  browser or account cookies.
