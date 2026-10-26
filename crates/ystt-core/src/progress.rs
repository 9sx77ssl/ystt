//! Unified progress events for single and batch downloads.
//!
//! Every event carries a `job_id` and, for batch items, the item index and batch
//! size. The same event stream is used by `download`, `download_with`,
//! `resolve_with` and `download_many`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::error::ErrorInfo;
use crate::model::MediaKind;

static NEXT_JOB_ID: AtomicU64 = AtomicU64::new(1);

/// Allocate a process-unique job ID.
pub(crate) fn next_job_id() -> u64 {
    NEXT_JOB_ID.fetch_add(1, Ordering::Relaxed)
}

/// Top-level event delivered to a progress sink.
#[derive(Debug, Clone, Serialize)]
pub struct ProgressEvent {
    /// Process-unique job ID. All events of one single download or one batch
    /// share this ID.
    pub job_id: u64,
    /// Source URL for an item event; `None` for batch-level events.
    pub url: Option<String>,
    /// Zero-based item index inside a batch; `None` for single downloads and
    /// batch-level events.
    pub batch_index: Option<usize>,
    /// Number of items in the batch; `None` for single downloads.
    pub batch_total: Option<usize>,
    /// Activity that occurred.
    pub stage: ProgressStage,
}

/// Activity inside a job.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ProgressStage {
    /// A batch started.
    BatchStarted {
        /// Number of URLs in the batch.
        total_items: usize,
    },
    /// URL/ID resolution started.
    Resolving,
    /// Metadata was resolved.
    Resolved {
        /// Post ID.
        post_id: String,
        /// Number of media items.
        media_count: usize,
    },
    /// A fresh file-cache manifest was used.
    CacheHit {
        /// Post ID.
        post_id: String,
    },
    /// A WAF challenge was detected.
    WafChallenge {
        /// Page attempt number.
        attempt: u32,
    },
    /// A WAF challenge was solved.
    WafSolved {
        /// Solver wall time.
        elapsed_ms: u64,
    },
    /// One media item started.
    MediaStarted {
        /// Media kind.
        kind: MediaKind,
        /// Gallery index, when applicable.
        index: Option<usize>,
        /// Total bytes when known.
        total_bytes: Option<u64>,
    },
    /// Bytes were written for one media item.
    MediaProgress {
        /// Media kind.
        kind: MediaKind,
        /// Gallery index, when applicable.
        index: Option<usize>,
        /// Bytes written so far.
        downloaded_bytes: u64,
        /// Total bytes when known.
        total_bytes: Option<u64>,
    },
    /// One media item completed.
    MediaCompleted {
        /// Media kind.
        kind: MediaKind,
        /// Gallery index, when applicable.
        index: Option<usize>,
        /// Final size.
        bytes: u64,
    },
    /// A single download or one batch item completed.
    Completed {
        /// End-to-end milliseconds for the item.
        total_ms: u64,
    },
    /// An item or single download failed.
    Failed {
        /// Stable error payload.
        error: ErrorInfo,
    },
    /// A batch item finished; aggregate item progress.
    BatchProgress {
        /// Items that succeeded so far.
        completed_items: usize,
        /// Items that failed so far.
        failed_items: usize,
        /// Total items.
        total_items: usize,
    },
    /// A batch completed.
    BatchCompleted {
        /// End-to-end wall time.
        total_ms: u64,
        /// Successful items.
        successes: usize,
        /// Failed items.
        failures: usize,
    },
}

/// Media item identity used by progress events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaTarget {
    /// Media kind.
    pub kind: MediaKind,
    /// Gallery index.
    pub index: Option<usize>,
}

impl MediaTarget {
    /// Video target.
    pub fn video() -> Self {
        Self {
            kind: MediaKind::Video,
            index: None,
        }
    }

    /// Gallery item.
    pub fn photo(index: usize) -> Self {
        Self {
            kind: MediaKind::Photo,
            index: Some(index),
        }
    }

    /// Audio target.
    pub fn audio() -> Self {
        Self {
            kind: MediaKind::Audio,
            index: None,
        }
    }
}

/// Any synchronous progress consumer.
pub trait ProgressSink: Send + Sync {
    /// Receive one event.
    fn report(&self, event: ProgressEvent);
}

impl<F> ProgressSink for F
where
    F: Fn(ProgressEvent) + Send + Sync,
{
    fn report(&self, event: ProgressEvent) {
        self(event)
    }
}

/// Shared progress reporter.
pub type ProgressReporter = Arc<dyn ProgressSink>;

/// Create a channel-backed reporter and receiver.
pub fn progress_channel() -> (ProgressReporter, UnboundedReceiver<ProgressEvent>) {
    let (sender, receiver) = unbounded_channel();
    (Arc::new(ChannelSink { sender }), receiver)
}

struct ChannelSink {
    sender: UnboundedSender<ProgressEvent>,
}

impl ProgressSink for ChannelSink {
    fn report(&self, event: ProgressEvent) {
        let _ = self.sender.send(event);
    }
}

/// Internal context-aware reporter.
#[derive(Clone)]
pub(crate) struct ProgressEmitter {
    reporter: Option<ProgressReporter>,
    job_id: u64,
    url: Option<String>,
    batch_index: Option<usize>,
    batch_total: Option<usize>,
}

impl ProgressEmitter {
    /// Context for a single download or resolve.
    pub(crate) fn single(
        reporter: Option<ProgressReporter>,
        job_id: u64,
        url: impl Into<String>,
    ) -> Self {
        Self {
            reporter,
            job_id,
            url: Some(url.into()),
            batch_index: None,
            batch_total: None,
        }
    }

    /// Context for batch-level events.
    pub(crate) fn batch(
        reporter: Option<ProgressReporter>,
        job_id: u64,
        total_items: usize,
    ) -> Self {
        Self {
            reporter,
            job_id,
            url: None,
            batch_index: None,
            batch_total: Some(total_items),
        }
    }

    /// Clone this context for one batch item.
    pub(crate) fn child(&self, url: impl Into<String>, index: usize, total: usize) -> Self {
        Self {
            reporter: self.reporter.clone(),
            job_id: self.job_id,
            url: Some(url.into()),
            batch_index: Some(index),
            batch_total: Some(total),
        }
    }

    /// Emit one stage.
    pub(crate) fn emit(&self, stage: ProgressStage) {
        let Some(reporter) = &self.reporter else {
            return;
        };
        reporter.report(ProgressEvent {
            job_id: self.job_id,
            url: self.url.clone(),
            batch_index: self.batch_index,
            batch_total: self.batch_total,
            stage,
        });
    }
}

/// Internal per-file progress tracker.
pub(crate) struct ProgressTracker {
    emitter: ProgressEmitter,
    target: MediaTarget,
    total: AtomicU64,
    downloaded: AtomicU64,
    last_reported: AtomicU64,
}

impl ProgressTracker {
    pub(crate) fn new(
        emitter: ProgressEmitter,
        target: MediaTarget,
        total: Option<u64>,
    ) -> Arc<Self> {
        let tracker = Arc::new(Self {
            emitter,
            target,
            total: AtomicU64::new(total.unwrap_or(0)),
            downloaded: AtomicU64::new(0),
            last_reported: AtomicU64::new(0),
        });
        tracker.emitter.emit(ProgressStage::MediaStarted {
            kind: target.kind,
            index: target.index,
            total_bytes: total,
        });
        tracker
    }

    pub(crate) fn set_total(&self, total: u64) {
        self.total.store(total, Ordering::Relaxed);
    }

    pub(crate) fn add(&self, bytes: u64) {
        let downloaded = self.downloaded.fetch_add(bytes, Ordering::Relaxed) + bytes;
        let last = self.last_reported.load(Ordering::Relaxed);
        if downloaded.saturating_sub(last) >= 128 * 1024 {
            self.last_reported.store(downloaded, Ordering::Relaxed);
            self.report(downloaded);
        }
    }

    pub(crate) fn complete(&self) {
        let downloaded = self.downloaded.load(Ordering::Relaxed);
        self.report(downloaded);
        self.emitter.emit(ProgressStage::MediaCompleted {
            kind: self.target.kind,
            index: self.target.index,
            bytes: downloaded,
        });
    }

    fn report(&self, downloaded: u64) {
        let total = self.total.load(Ordering::Relaxed);
        self.emitter.emit(ProgressStage::MediaProgress {
            kind: self.target.kind,
            index: self.target.index,
            downloaded_bytes: downloaded,
            total_bytes: (total > 0).then_some(total),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn channel_reports_context_and_stage() {
        let (reporter, mut receiver) = progress_channel();
        let emitter = ProgressEmitter::single(Some(reporter), 7, "https://tiktok/x");
        emitter.emit(ProgressStage::Resolving);
        let event = receiver.try_recv().unwrap();
        assert_eq!(event.job_id, 7);
        assert_eq!(event.url.as_deref(), Some("https://tiktok/x"));
        assert!(matches!(event.stage, ProgressStage::Resolving));
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["stage"]["event"], "resolving");
        assert_eq!(json["job_id"], 7);
    }

    #[test]
    fn tracker_reports_start_progress_and_complete() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let events = events.clone();
            Arc::new(move |event| events.lock().unwrap().push(event))
        };
        let emitter = ProgressEmitter::single(Some(sink), 1, "https://tiktok/x");
        let tracker = ProgressTracker::new(emitter, MediaTarget::video(), Some(300 * 1024));
        tracker.add(1024);
        tracker.add(300 * 1024);
        tracker.complete();
        let events = events.lock().unwrap();
        assert!(matches!(
            events[0].stage,
            ProgressStage::MediaStarted { .. }
        ));
        assert!(
            events
                .iter()
                .any(|event| matches!(event.stage, ProgressStage::MediaProgress { .. }))
        );
        assert!(matches!(
            events.last().unwrap().stage,
            ProgressStage::MediaCompleted { .. }
        ));
    }

    #[test]
    fn batch_child_carries_index() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let events = events.clone();
            Arc::new(move |event| events.lock().unwrap().push(event))
        };
        let root = ProgressEmitter::batch(Some(sink), 9, 2);
        root.child("https://tiktok/a", 1, 2)
            .emit(ProgressStage::Resolving);
        let event = &events.lock().unwrap()[0];
        assert_eq!(event.job_id, 9);
        assert_eq!(event.batch_index, Some(1));
        assert_eq!(event.batch_total, Some(2));
    }
}
