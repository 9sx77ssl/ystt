#!/usr/bin/env bun
/**
 * Bun-only OpenAPI sync. Fetches the live schema with a Bearer key, validates
 * it with scripts/check-openapi.ts, then writes pretty JSON to the repo root.
 *
 * Usage:
 *   YSTT_API_KEY=... bun scripts/sync-openapi.ts [openapi.json]
 */

const output = Bun.argv[2] ?? "openapi.json";
const key = process.env.YSTT_API_KEY;
if (!key) {
  console.error("sync-openapi: YSTT_API_KEY is required");
  process.exit(1);
}
const base = (process.env.YSTT_API_URL ?? "https://ystt.d9v.uk/api").replace(/\/+$/, "");
const response = await fetch(`${base}/v1/openapi.json`, {
  headers: { Authorization: `Bearer ${key}` },
});
if (!response.ok) {
  console.error(`sync-openapi: HTTP ${response.status}`);
  process.exit(1);
}
const document = await response.json();
const text = JSON.stringify(document, null, 2) + "\n";
const temporary = `${output}.sync-${process.pid}.tmp`;
await Bun.write(temporary, text);

const check = Bun.spawnSync(["bun", "scripts/check-openapi.ts", temporary], {
  stdout: "inherit",
  stderr: "inherit",
});
if (check.exitCode !== 0) {
  await Bun.file(temporary).delete();
  process.exit(check.exitCode ?? 1);
}
await Bun.write(output, text);
await Bun.file(temporary).delete();
console.log(`sync-openapi: wrote ${output}`);
