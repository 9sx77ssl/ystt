# ystt HTTP API

`ystt-api` is a typed Axum service over the same in-memory pipeline used by the
Telegram bot. It is both a binary and a library.

- Public base: `https://ystt.d9v.uk/api`
- Local bind: `127.0.0.1:8091`
- All `/v1` endpoints require `Authorization: Bearer <YSTT_API_KEY>`.
- Media bytes and metadata are held in a bounded process-local store; no media
  or cache files are written to disk.
- OpenAPI 3.1 is generated at runtime from the Rust DTOs:
  `GET /api/v1/openapi.json` (with the Bearer key).

## Endpoints

| Method | Path | Description |
| --- | --- | --- |
| GET | `/v1/health` | auth and liveness probe |
| POST | `/v1/resolve` | resolve a URL, return `MediaInfo` only |
| POST | `/v1/download` | download one post into memory |
| POST | `/v1/batch` | ordered batch download |
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

`max_bytes` cannot exceed the server cap. The response contains typed
`MediaDescriptor` objects:

```json
{
  "post_id": "123",
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
    "https://www.tiktok.com/@b/video/2"
  ],
  "concurrency": 2,
  "include_audio": false
}
```

Items are returned in input order. Individual failures do not abort other items.

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
`file_too_large`, `partial`, `network`, and `internal`.

## Runtime limits

- Bearer key: required on every route.
- Rate limit: 120 requests/minute per client, burst 30.
- Download workers: 3; batch concurrency: 2.
- Per-media cap: 128 MiB.
- In-memory media store: 768 MiB, TTL 900 seconds.
- Media tokens are 128-bit random values and are evicted by TTL/size.
- A bounded URL/options -> response cache (512 entries) returns `cache_hit: true`
  and skips TikTok entirely while all referenced media tokens are still live.
- Range requests are supported by `/v1/media/{token}`.

## Configuration

| Variable | Default | Meaning |
| --- | --- | --- |
| `YSTT_API_KEY` | required | Bearer key |
| `YSTT_API_BIND` | `127.0.0.1:8091` | local bind |
| `YSTT_API_PUBLIC_BASE` | `https://ystt.d9v.uk/api` | public URL prefix |
| `YSTT_API_MAX_MB` | `128` | per-media cap |
| `YSTT_API_MEDIA_TTL` | `900` | media token TTL seconds |
| `YSTT_API_STORE_MB` | `768` | process-local media cap |
| `YSTT_API_WORKERS` | `3` | concurrent downloads |
| `YSTT_API_BATCH_CONCURRENCY` | `2` | batch parallelism |
| `YSTT_API_MAX_BATCH` | `20` | max batch URLs |
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
- For stricter edge protection, enable Cloudflare WAF managed rules, a
  `/api/*` rate-limit rule, Always Use HTTPS and HSTS in the Cloudflare
  dashboard. The repository cannot set zone settings without a Cloudflare API
  token.
