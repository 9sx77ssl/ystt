#!/usr/bin/env bun
/**
 * Bun-only OpenAPI contract check. No Node, no npx, no external validator.
 *
 * Usage:
 *   bun scripts/check-openapi.ts [path/to/openapi.json]
 */

const path = Bun.argv[2] ?? "openapi.json";
const text = await Bun.file(path).text();

function fail(message: string): never {
  console.error(`openapi-check: ${message}`);
  process.exit(1);
}

let doc: any;
try {
  doc = JSON.parse(text);
} catch (error) {
  fail(`${path} is not valid JSON: ${String(error)}`);
}

if (doc.openapi !== "3.1.0") fail(`unexpected OpenAPI version: ${doc.openapi}`);
if (!doc.info?.title || !doc.info?.version) fail("info.title/info.version missing");
if (!Array.isArray(doc.servers) || doc.servers.length === 0) fail("servers missing");
if (!doc.components?.schemas || Object.keys(doc.components.schemas).length === 0) {
  fail("components.schemas missing");
}
if (!doc.components.securitySchemes?.bearer_auth) fail("bearer auth scheme missing");
if (!Array.isArray(doc.security) || doc.security.length === 0) fail("global security missing");
if (!text.startsWith("{\n  \"")) fail("file is not pretty-printed with two-space indentation");
if (!text.endsWith("\n")) fail("file must end with a newline");

const methods = new Set(["get", "post", "put", "patch", "delete", "head", "options", "trace"]);
const operationIds = new Set<string>();
const refs = new Set<string>();

function collectRefs(value: any): void {
  if (Array.isArray(value)) {
    for (const item of value) collectRefs(item);
    return;
  }
  if (!value || typeof value !== "object") return;
  if (typeof value.$ref === "string") refs.add(value.$ref);
  for (const child of Object.values(value)) collectRefs(child);
}

function resolveRef(ref: string): boolean {
  if (!ref.startsWith("#/")) return false;
  let current: any = doc;
  for (const raw of ref.slice(2).split("/")) {
    const key = raw.replaceAll("~1", "/").replaceAll("~0", "~");
    if (!current || typeof current !== "object" || !(key in current)) return false;
    current = current[key];
  }
  return true;
}

collectRefs(doc);
for (const ref of refs) {
  if (!resolveRef(ref)) fail(`unresolved $ref: ${ref}`);
}

const paths = doc.paths ?? {};
for (const [pathName, pathItem] of Object.entries<any>(paths)) {
  if (!pathName.startsWith("/v1/")) fail(`unexpected non-/v1 path: ${pathName}`);
  for (const [method, operation] of Object.entries<any>(pathItem)) {
    if (!methods.has(method)) continue;
    if (!operation.operationId) fail(`${method.toUpperCase()} ${pathName} has no operationId`);
    if (operationIds.has(operation.operationId)) fail(`duplicate operationId: ${operation.operationId}`);
    operationIds.add(operation.operationId);
    const responses = operation.responses ?? {};
    if (Object.keys(responses).length === 0) fail(`${method.toUpperCase()} ${pathName} has no responses`);
    for (const [status, response] of Object.entries<any>(responses)) {
      if (!response || typeof response.description !== "string" || !response.description.trim()) {
        fail(`${method.toUpperCase()} ${pathName} ${status} has no response description`);
      }
    }
    if (!Array.isArray(operation.security) || operation.security.length === 0) {
      fail(`${method.toUpperCase()} ${pathName} has no operation security`);
    }
  }
}

for (const requiredPath of [
  "/v1/health",
  "/v1/resolve",
  "/v1/download",
  "/v1/batch",
  "/v1/stats",
  "/v1/openapi.json",
  "/v1/media/{token}",
]) {
  if (!paths[requiredPath]) fail(`required path missing: ${requiredPath}`);
}
if (!paths["/v1/media/{token}"].get || !paths["/v1/media/{token}"].head) {
  fail("media GET and HEAD must both be documented");
}

const schemas = doc.components.schemas;
for (const name of [
  "Provider",
  "DownloadResponse",
  "MediaDescriptor",
  "PlaylistDescriptor",
  "ResolveResponse",
  "SoundCloudEntity",
  "SoundCloudTrack",
]) {
  if (!schemas[name]) fail(`required schema missing: ${name}`);
}
const cacheHit = schemas.DownloadResponse?.properties?.cache_hit?.description;
if (typeof cacheHit !== "string" || cacheHit.toLowerCase().includes("always false")) {
  fail("DownloadResponse.cache_hit description is stale");
}

console.log(
  `openapi-check: ok — ${path}, OpenAPI ${doc.openapi}, ` +
    `${Object.keys(paths).length} paths, ${operationIds.size} operations, ` +
    `${Object.keys(schemas).length} schemas, ${refs.size} refs`,
);
