#!/usr/bin/env bash
# Open the live ystt OpenAPI schema in the local browser without putting the
# Bearer key into the URL, history or repository.
set -euo pipefail

host="${YSTT_API_HOST:-root@146.103.50.217}"
base="${YSTT_API_URL:-https://ystt.d9v.uk/api}"

if [[ -n "${YSTT_API_KEY:-}" ]]; then
  key="$YSTT_API_KEY"
else
  key=$(ssh "$host" 'grep ^YSTT_API_KEY= /etc/ystt-api/api.env | cut -d= -f2-')
fi

tmpdir="${TMPDIR:-/tmp}/ystt-openapi-viewer"
mkdir -p "$tmpdir"
chmod 700 "$tmpdir"
json="$tmpdir/openapi.json"
html="$tmpdir/openapi.html"

curl -fsS -H "Authorization: Bearer $key" "$base/v1/openapi.json" -o "$json"
chmod 600 "$json"

python3 - "$json" "$html" <<'PY'
import html
import json
import sys
from datetime import datetime, timezone

source, destination = sys.argv[1:3]
data = json.load(open(source, encoding="utf-8"))
pretty = json.dumps(data, indent=2, ensure_ascii=False)
title = html.escape(data.get("info", {}).get("title", "OpenAPI"))
version = html.escape(data.get("info", {}).get("version", ""))
page = f"""<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title} {version}</title>
<style>
:root {{ color-scheme: light dark; }}
body {{ margin: 0; padding: 24px; font: 14px/1.45 ui-monospace, SFMono-Regular, Menlo, monospace; }}
h1 {{ font: 600 20px/1.2 system-ui, sans-serif; margin: 0 0 8px; }}
p {{ margin: 0 0 20px; font: 13px system-ui, sans-serif; opacity: .7; }}
pre {{ white-space: pre-wrap; overflow-wrap: anywhere; tab-size: 2; }}
</style>
</head>
<body>
<h1>{title} <small>{version}</small></h1>
<p>Live schema fetched {datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M:%S UTC")}; key is not included in this page.</p>
<pre>{html.escape(pretty)}</pre>
</body>
</html>
"""
open(destination, "w", encoding="utf-8").write(page)
PY
chmod 600 "$html"

if command -v xdg-open >/dev/null 2>&1; then
  xdg-open "$html" >/dev/null 2>&1 &
fi
echo "$html"
