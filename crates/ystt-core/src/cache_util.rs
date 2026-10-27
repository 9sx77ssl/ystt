//! Small shared filesystem helpers for cache modules.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::io::AsyncWriteExt;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
const STALE_LOCK: Duration = Duration::from_secs(30 * 60);

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

/// Exclusive cross-process cache lock.
pub(crate) struct FileLock {
    path: PathBuf,
    token: String,
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let owned = std::fs::read_to_string(&self.path)
            .map(|contents| contents.contains(&self.token))
            .unwrap_or(false);
        if owned {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Acquire an exclusive lock file, waiting for another process to finish.
pub(crate) async fn acquire_lock(path: &Path) -> std::io::Result<FileLock> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let started = Instant::now();
    loop {
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .await
        {
            Ok(mut file) => {
                let token = format!(
                    "pid={} at={} counter={}",
                    std::process::id(),
                    unix_now(),
                    TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
                );
                file.write_all(format!("{token}\n").as_bytes()).await?;
                file.flush().await?;
                return Ok(FileLock {
                    path: path.to_owned(),
                    token,
                });
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                if let Ok(metadata) = tokio::fs::metadata(path).await {
                    let age = metadata
                        .modified()
                        .ok()
                        .and_then(|modified| modified.elapsed().ok())
                        .unwrap_or_default();
                    if age > STALE_LOCK || (metadata.len() == 0 && age > Duration::from_secs(5)) {
                        let _ = tokio::fs::remove_file(path).await;
                        continue;
                    }
                }
                if started.elapsed() > STALE_LOCK {
                    return Err(std::io::Error::new(
                        ErrorKind::TimedOut,
                        format!("timed out waiting for cache lock {}", path.display()),
                    ));
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => return Err(error),
        }
    }
}
