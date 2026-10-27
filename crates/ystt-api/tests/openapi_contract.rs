//! OpenAPI contract and snapshot checks.
//!
//! This runs without a browser, Node or npx. It is the Rust half of the
//! Bun-only schema workflow (`bun scripts/check-openapi.ts` is the other half).

use std::collections::HashSet;
use std::fs;

use serde_json::Value;
use utoipa::OpenApi;
use ystt_api::openapi::ApiDoc;

const METHODS: &[&str] = &[
    "get", "post", "put", "patch", "delete", "head", "options", "trace",
];

fn operation_ids(doc: &Value) -> Vec<&str> {
    let mut ids = Vec::new();
    for item in doc["paths"].as_object().unwrap().values() {
        for method in METHODS {
            if let Some(operation) = item.get(*method) {
                ids.push(operation["operationId"].as_str().unwrap());
            }
        }
    }
    ids
}

fn collect_refs<'a>(value: &'a Value, refs: &mut HashSet<&'a str>) {
    match value {
        Value::Object(object) => {
            if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
                refs.insert(reference);
            }
            for child in object.values() {
                collect_refs(child, refs);
            }
        }
        Value::Array(array) => {
            for child in array {
                collect_refs(child, refs);
            }
        }
        _ => {}
    }
}

fn resolve_ref(doc: &Value, reference: &str) -> bool {
    let Some(pointer) = reference.strip_prefix("#/") else {
        return false;
    };
    let mut current = doc;
    for part in pointer.split('/') {
        let part = part.replace("~1", "/").replace("~0", "~");
        let Some(next) = current.get(&part) else {
            return false;
        };
        current = next;
    }
    true
}

#[test]
fn generated_document_is_contract_valid_and_committed() {
    let document = ApiDoc::openapi();
    let generated = serde_json::to_string_pretty(&document).unwrap() + "\n";
    let committed_path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../openapi.json");
    let committed = fs::read_to_string(committed_path).unwrap_or_else(|error| {
        panic!("cannot read {committed_path}: {error}; run `bun scripts/sync-openapi.ts`")
    });
    assert_eq!(
        committed, generated,
        "openapi.json is stale; run `bun scripts/sync-openapi.ts` (Bun only)"
    );

    let doc: Value = serde_json::from_str(&generated).unwrap();
    assert_eq!(doc["openapi"], "3.1.0");
    assert_eq!(doc["info"]["version"], env!("CARGO_PKG_VERSION"));
    assert!(
        doc["servers"]
            .as_array()
            .is_some_and(|servers| !servers.is_empty())
    );
    assert_eq!(
        doc["components"]["securitySchemes"]["bearer_auth"]["scheme"],
        "bearer"
    );
    assert!(
        doc["security"]
            .as_array()
            .is_some_and(|security| !security.is_empty())
    );

    let mut operation_ids = operation_ids(&doc);
    let operation_count = operation_ids.len();
    assert!(operation_count >= 8);
    operation_ids.sort_unstable();
    operation_ids.dedup();
    assert_eq!(
        operation_ids.len(),
        operation_count,
        "duplicate operationId"
    );

    let paths = doc["paths"].as_object().unwrap();
    for (path, item) in paths {
        assert!(path.starts_with("/v1/"), "unexpected path: {path}");
        for method in METHODS {
            let Some(operation) = item.get(*method) else {
                continue;
            };
            assert!(
                operation["operationId"]
                    .as_str()
                    .is_some_and(|id| !id.is_empty()),
                "{method} {path} has no operationId"
            );
            assert!(
                operation["security"]
                    .as_array()
                    .is_some_and(|security| !security.is_empty()),
                "{method} {path} has no security requirement"
            );
            for (status, response) in operation["responses"].as_object().unwrap() {
                assert!(
                    response["description"]
                        .as_str()
                        .is_some_and(|description| !description.trim().is_empty()),
                    "{method} {path} {status} has no response description"
                );
            }
        }
    }

    for required in [
        "/v1/health",
        "/v1/resolve",
        "/v1/download",
        "/v1/batch",
        "/v1/stats",
        "/v1/openapi.json",
        "/v1/media/{token}",
    ] {
        assert!(paths.contains_key(required), "missing path: {required}");
    }
    assert!(paths["/v1/media/{token}"].get("get").is_some());
    assert!(paths["/v1/media/{token}"].get("head").is_some());

    let schemas = doc["components"]["schemas"].as_object().unwrap();
    for required in [
        "Provider",
        "DownloadResponse",
        "MediaDescriptor",
        "PlaylistDescriptor",
        "ResolveResponse",
        "SoundCloudEntity",
        "SoundCloudTrack",
    ] {
        assert!(schemas.contains_key(required), "missing schema: {required}");
    }
    assert!(
        !schemas["DownloadResponse"]["properties"]["cache_hit"]["description"]
            .as_str()
            .unwrap_or_default()
            .to_ascii_lowercase()
            .contains("always false")
    );

    let mut refs = HashSet::new();
    collect_refs(&doc, &mut refs);
    assert!(!refs.is_empty());
    for reference in refs {
        assert!(
            resolve_ref(&doc, reference),
            "unresolved reference: {reference}"
        );
    }
}
