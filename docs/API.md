# ystt HTTP API

`ystt-api` is a typed Axum service over the same in-memory TikTok + SoundCloud
pipeline used by the Telegram bot. It is both a binary and a library.

- Public base: `https://ystt.d9v.uk/api`
- Local bind: `127.0.0.1:8091`
- All `/v1` endpoints require `Authorization: Bearer <YSTT_API_KEY>`.
  On the server the key is in `/etc/ystt-api/api.env` (`0600`), never in git.
- Media bytes and metadata are held in a bounded process-local store; no media
  or cache files are written to disk.
- OpenAPI 3.1 is generated at runtime from the Rust DTOs:
  `GET /api/v1/openapi.json` (with the Bearer key). The endpoint returns
  pretty-printed JSON with a trailing newline.
- The Rust snapshot/contract test catches stale root schemas and missing
  response descriptions.

## Endpoints

| Method | Path | Description |
| --- | --- | --- |
| GET | `/v1/health` | auth and liveness probe |
| POST | `/v1/resolve` | resolve a TikTok/SoundCloud URL (provider-tagged) |
| POST | `/v1/download` | download one TikTok post or SoundCloud track/playlist |
| POST | `/v1/batch` | ordered mixed-provider batch download |
| GET/HEAD | `/v1/media/{token}` | stream media bytes, Range supported |
| GET | `/v1/stats` | media-store statistics |
| GET | `/v1/openapi.json` | generated OpenAPI document |

## Download request

```json
{
  "url": "https://www.tiktok.com/@user/video/123",
  "quality": "balanced",
  "include_audio": true,
  "allow_partial": true,
  "max_bytes": 134217728,
  "parallel_ranges": true,
  "max_connections": 6,
  "photo_concurrency": 8,
  "adaptive_ranges": true,
  "media_retries": 2
}
```

`max_bytes` cannot exceed the server cap. TikTok and SoundCloud are auto-detected
from the URL. The response contains typed `MediaDescriptor` objects:

```json
{
  "provider": "tiktok",
  "post_id": "123",
  "title": null,
  "artwork_url": null,
  "video": {
    "token": "…",
    "url": "https://ystt.d9v.uk/api/v1/media/…",
    "kind": "video",
    "mime": "video/mp4",
    "ext": "mp4",
    "size": 31828479,
    "sha256": "196ffa…",
    "source_url": "https://…",
    "width": 576,
    "height": 1024,
    "duration_sec": 200
  },
  "photos": [],
  "audio": null,
  "failures": [],
  "timings": { "resolve_ms": 596, "media_ms": 141, "total_ms": 737 },
  "cache_hit": false
}
```

A SoundCloud track uses the same response with `provider: "soundcloud"`,
`audio` set, and `title`/`artwork_url` populated:

```json
{
  "provider": "soundcloud",
  "post_id": "1407903748",
  "title": "Track title",
  "artwork_url": "https://i1.sndcdn.com/...-t300x300.jpg",
  "author": { "id": "Artist", "nickname": "Artist" },
  "audio": {
    "token": "…",
    "kind": "audio",
    "mime": "audio/mp4",
    "ext": "m4a",
    "size": 4371992,
    "title": "Track title",
    "artist": "Artist",
    "duration_sec": 216
  },
  "photos": [],
  "playlist": null,
  "artwork": {
    "token": "…",
    "kind": "photo",
    "mime": "image/jpeg",
    "ext": "jpg",
    "size": 47988,
    "width": 300,
    "height": 300
  }
}
```

A SoundCloud `/sets/` URL returns a `playlist` descriptor with per-track
`audio` descriptors, an authenticated playlist `artwork` token, original order
and per-track failures:

```json
{
  "provider": "soundcloud",
  "post_id": "2297180103",
  "title": "Shh!",
  "playlist": {
    "playlist_id": "2297180103",
    "title": "Shh!",
    "artist": "Kai Angel & 9mice",
    "track_count": 15,
    "artwork": { "token": "…", "kind": "photo", "mime": "image/jpeg", "ext": "jpg" },
    "tracks": [
      { "track_id": "…", "title": "…", "artist": "…", "audio": { "token": "…", "kind": "audio", "ext": "m4a" } }
    ],
    "failures": []
  }
}
```

For `/v1/resolve`, the top-level object has `provider` and exactly one of:
- flattened TikTok `MediaInfo` fields (existing compatibility), or
- `soundcloud: { type: "track" | "playlist", data: … }`.

Fetch the bytes with the same Bearer key:

```bash
curl -H "Authorization: Bearer $YSTT_API_KEY" \
  -H 'Range: bytes=0-1048575' \
  "https://ystt.d9v.uk/api/v1/media/$TOKEN" -o first.part
```

## Batch

```json
{
  "urls": [
    "https://www.tiktok.com/@a/video/1",
    "https://soundcloud.com/a/track",
    "https://soundcloud.com/b/sets/playlist"
  ],
  "concurrency": 2,
  "item_timeout_ms": 120000,
  "item_retries": 2,
  "include_audio": false
}
```

Items are returned in input order. Individual failures do not abort other items.
Each item is provider-tagged. A playlist item expands to its tracks inside that
one item; it does not change the batch item count.

## Errors

All non-2xx responses use:

```json
{
  "error": {
    "code": "unsupported_url",
    "message": "not a TikTok URL: https://example.com"
  }
}
```

Stable error codes include `unauthorized`, `rate_limited`, `invalid_url`,
`unsupported_url`, `unavailable`, `age_restricted`, `no_media`, `no_video`,
`file_too_large`, `file_exists`, `timeout`, `partial`, `network`, and `internal`.
Errors may carry a non-secret `context` object with `provider`, `host`, `kind`,
`status` and `attempt` when known.

## Runtime limits

- Bearer key: required on every route.
- Rate limit: 120 requests/minute per client, burst 30.
- Download workers: 3; batch concurrency: 2.
- Per-media cap: 128 MiB.
- In-memory media store: 768 MiB default, 1024 MiB on the production host, TTL
  900 seconds. A 184-track playlist measured ~680 MiB, so large playlists need
  the larger cap.
- SoundCloud playlist cap: 200 tracks per request.
- Artwork is returned as both `artwork_url` (source URL) and an authenticated
  `artwork` token. The token is a normal short-lived media object.
- Media tokens are 128-bit random values and are evicted by TTL/size.
- A bounded provider-aware URL/options -> response cache (512 entries) returns
  `cache_hit: true` and skips the network entirely while all referenced media
  tokens are still live.
- Range requests are supported by `/v1/media/{token}`.

## Configuration

| Variable | Default | Meaning |
| --- | --- | --- |
| `YSTT_API_KEY` | required | Bearer key |
| `YSTT_API_BIND` | `127.0.0.1:8091` | local bind |
| `YSTT_API_PUBLIC_BASE` | `https://ystt.d9v.uk/api` | public URL prefix |
| `YSTT_API_MAX_MB` | `128` | per-media cap |
| `YSTT_API_MEDIA_TTL` | `900` | media token TTL seconds |
| `YSTT_API_STORE_MB` | `768` (`1024` production) | process-local media cap |
| `YSTT_API_WORKERS` | `3` | concurrent downloads |
| `YSTT_API_BATCH_CONCURRENCY` | `2` | batch parallelism |
| `YSTT_API_MAX_BATCH` | `20` | max batch URLs |
| `YSTT_API_MAX_PLAYLIST_TRACKS` | `200` | max tracks in one SoundCloud playlist |
| `YSTT_API_RATE_PER_MIN` | `120` | per-client rate |
| `YSTT_API_RATE_BURST` | `30` | token-bucket burst |

## Deployment

```bash
cargo build --release -p ystt-api
install -m 0755 target/release/ystt-api /opt/ystt-api/ystt-api
install -m 0644 deploy/ystt-api.service /etc/systemd/system/ystt-api.service
install -d -m 0700 /etc/ystt-api
cp deploy/api.env.example /etc/ystt-api/api.env
# set YSTT_API_KEY to a random 32-byte hex value
systemctl daemon-reload
systemctl enable --now ystt-api
```

Nginx needs the `/api/` location from `deploy/nginx-ystt-api.conf`. The API
process listens only on loopback; TLS terminates at Nginx.

## TLS, Cloudflare and HTTP/2

- Client -> Cloudflare: HTTPS, TLS 1.3, HTTP/2 and advertised HTTP/3.
- Cloudflare -> origin: HTTPS, origin Nginx TLS 1.2/1.3 with modern ECDHE ciphers.
- Nginx -> API: loopback HTTP/1.1 with upstream keepalive. This is intentional;
  TLS and HTTP/2 termination happen at the public edge, not on localhost.
- Nginx serves HTTP/2 (`http2 on;`) and has a 64-connection upstream keepalive.
- Media uses `private, max-age=600`; JSON API responses are not cached by
  Cloudflare (`cf-cache-status: DYNAMIC` in production).
- Nginx adds HSTS, `nosniff`, `Referrer-Policy`, `X-Frame-Options` and
  `Permissions-Policy`.
- Nginx rate-limits `/api/` per `CF-Connecting-IP` (20 r/s, burst 40, 20
  connections per client) before requests reach the API's own 120/minute token
  bucket.
- The API result cache is process-local and intentionally not shared with the
  CLI file cache, Telegram `file_id` cache or Redis.
- For stricter edge protection, enable Cloudflare WAF managed rules, a
  `/api/*` rate-limit rule, Always Use HTTPS and HSTS in the Cloudflare
  dashboard. The repository cannot set zone settings without a Cloudflare API
  token.
