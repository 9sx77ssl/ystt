//! Small shared filesystem helpers for cache modules.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Normalize a cache key into a safe filename stem.
pub(crate) fn sanitize_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for character in key.chars() {
        if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
            out.push(character);
        } else {
            out.push('_');
        }
    }
    out.truncate(160);
    out
}

/// Create a sibling temporary path with process and nano-time uniqueness.
pub(crate) fn temp_sibling(path: &Path) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos())
        .unwrap_or(0);
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "cache".into());
    path.with_file_name(format!(
        ".{name}.{}.{}.{}.tmp",
        std::process::id(),
        nanos,
        counter
    ))
}

/// Current Unix time in seconds.
pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}
