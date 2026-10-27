# ystt-api

Typed HTTP API over the in-memory `ystt` pipeline.

## Endpoints

All `/v1` routes require:

```http
Authorization: Bearer <YSTT_API_KEY>
```

Public base in production: `https://ystt.d9v.uk/api`

| Method | Path | Purpose |
| --- | --- | --- |
| GET | `/v1/health` | auth/health probe |
| POST | `/v1/resolve` | metadata only |
| POST | `/v1/download` | download one post into memory |
| POST | `/v1/batch` | ordered batch with bounded concurrency |
| GET/HEAD | `/v1/media/{token}` | media bytes, Range supported |
| GET | `/v1/stats` | media-store/queue stats |
| GET | `/v1/openapi.json` | generated OpenAPI 3.1 schema |

Media bytes are never written to disk. `/v1/download` returns short-lived
authenticated `token`/`url` descriptors; fetch them with the same Bearer key
before the TTL expires. The store is bounded and evicts oldest entries.

## Examples

```bash
export YSTT_API_KEY='...'
BASE='https://ystt.d9v.uk/api'

curl -sS -H "Authorization: Bearer $YSTT_API_KEY" \
  -H 'content-type: application/json' \
  -d '{"url":"https://www.tiktok.com/@user/video/123"}' \
  "$BASE/v1/download" | tee /tmp/ystt.json

TOKEN=$(jq -r '.video.token' /tmp/ystt.json)
curl -H "Authorization: Bearer $YSTT_API_KEY" \
  -o /tmp/video.mp4 "$BASE/v1/media/$TOKEN"
```

Use the same request with `include_audio: true` for music and batch mode for
multiple URLs. OpenAPI generation is derived from the Rust request/response
types, including status codes and the Bearer security scheme.

## Embedding

```rust
use ystt_api::{ApiConfig, AppState, router};

let state = AppState::new(ApiConfig::from_env()?)?;
let app = router(state);
```

The crate exposes its DTOs, store, downloader and router, so another Axum
service can mount the same API without a subprocess.
