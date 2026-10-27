#!/usr/bin/env bun
/**
 * Bun-only live OpenAPI viewer. Reads the Bearer key from YSTT_API_KEY or from
 * the server env over SSH, fetches the schema, writes a standalone HTML page
 * and opens it locally. The key is never put into the page or a URL.
 *
 * Usage:
 *   YSTT_API_KEY=... bun scripts/openapi-viewer.ts
 *   bun scripts/openapi-viewer.ts
 */

const host = process.env.YSTT_API_HOST ?? "root@146.103.50.217";
const base = (process.env.YSTT_API_URL ?? "https://ystt.d9v.uk/api").replace(/\/+$/, "");
let key = process.env.YSTT_API_KEY;
if (!key) {
  const processResult = Bun.spawnSync(
    ["ssh", host, "grep ^YSTT_API_KEY= /etc/ystt-api/api.env | cut -d= -f2-"],
    { stdout: "pipe", stderr: "pipe" },
  );
  if (processResult.exitCode !== 0) {
    console.error(`openapi-viewer: cannot read remote key: ${processResult.stderr.toString()}`);
    process.exit(1);
  }
  key = processResult.stdout.toString().trim();
}
if (!key) {
  console.error("openapi-viewer: empty API key");
  process.exit(1);
}

const response = await fetch(`${base}/v1/openapi.json`, {
  headers: { Authorization: `Bearer ${key}` },
});
if (!response.ok) {
  console.error(`openapi-viewer: HTTP ${response.status}`);
  process.exit(1);
}
const document: any = await response.json();
const pretty = JSON.stringify(document, null, 2);
const html = `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>${escapeHtml(document.info?.title ?? "OpenAPI")} ${escapeHtml(document.info?.version ?? "")}</title>
<style>
:root { color-scheme: light dark; }
body { margin: 0; padding: 24px; font: 14px/1.45 ui-monospace, SFMono-Regular, Menlo, monospace; }
h1 { font: 600 20px/1.2 system-ui, sans-serif; margin: 0 0 8px; }
p { margin: 0 0 20px; font: 13px system-ui, sans-serif; opacity: .7; }
pre { white-space: pre-wrap; overflow-wrap: anywhere; tab-size: 2; }
</style>
</head>
<body>
<h1>${escapeHtml(document.info?.title ?? "OpenAPI")} <small>${escapeHtml(document.info?.version ?? "")}</small></h1>
<p>Live schema fetched ${new Date().toISOString()}; key is not included in this page.</p>
<pre>${escapeHtml(pretty)}</pre>
</body>
</html>
`;

function escapeHtml(value: string): string {
  return value
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;");
}

const root = (process.env.TMPDIR ?? "/tmp").replace(/\/+$/, "") + "/ystt-openapi-viewer";
await Bun.spawnSync(["mkdir", "-p", root]);
const output = `${root}/openapi.html`;
await Bun.write(output, html);
await Bun.spawnSync(["chmod", "700", root]);
await Bun.spawnSync(["chmod", "600", output]);
if (Bun.which("xdg-open")) {
  Bun.spawn(["xdg-open", output], { stdout: "ignore", stderr: "ignore" });
}
console.log(output);
