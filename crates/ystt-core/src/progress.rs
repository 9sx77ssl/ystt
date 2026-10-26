//! Typed progress events for long downloads.
//!
//! Events are delivered through a [`ProgressSink`]. Use [`progress_channel`]
//! for a `tokio` channel, or implement the trait for a callback.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::model::MediaKind;

/// Stage of the download pipeline.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ProgressEvent {
    /// URL/ID resolution started.
    Resolving {
        /// Input URL.
        url: String,
    },
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
    /// The whole job completed.
    Completed {
        /// End-to-end milliseconds.
        total_ms: u64,
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

    /// Photo target.
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
        self(event);
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

/// Internal per-file progress tracker.
pub(crate) struct ProgressTracker {
    reporter: ProgressReporter,
    target: MediaTarget,
    total: AtomicU64,
    downloaded: AtomicU64,
    last_reported: AtomicU64,
}

impl ProgressTracker {
    pub(crate) fn new(
        reporter: ProgressReporter,
        target: MediaTarget,
        total: Option<u64>,
    ) -> Arc<Self> {
        let tracker = Arc::new(Self {
            reporter,
            target,
            total: AtomicU64::new(total.unwrap_or(0)),
            downloaded: AtomicU64::new(0),
            last_reported: AtomicU64::new(0),
        });
        tracker.reporter.report(ProgressEvent::MediaStarted {
            kind: tracker.target.kind,
            index: tracker.target.index,
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
        self.reporter.report(ProgressEvent::MediaCompleted {
            kind: self.target.kind,
            index: self.target.index,
            bytes: downloaded,
        });
    }

    fn report(&self, downloaded: u64) {
        let total = self.total.load(Ordering::Relaxed);
        self.reporter.report(ProgressEvent::MediaProgress {
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
    fn channel_reports_events() {
        let (reporter, mut receiver) = progress_channel();
        reporter.report(ProgressEvent::Resolving {
            url: "https://www.tiktok.com/@u/video/1".into(),
        });
        let event = receiver.try_recv().unwrap();
        assert!(matches!(event, ProgressEvent::Resolving { .. }));
    }

    #[test]
    fn tracker_reports_start_progress_and_complete() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let events = events.clone();
            Arc::new(move |event| events.lock().unwrap().push(event))
        };
        let tracker = ProgressTracker::new(sink, MediaTarget::video(), Some(300 * 1024));
        tracker.add(1024);
        tracker.add(300 * 1024);
        tracker.complete();
        let events = events.lock().unwrap();
        assert!(matches!(events[0], ProgressEvent::MediaStarted { .. }));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ProgressEvent::MediaProgress { .. }))
        );
        assert!(matches!(
            events.last().unwrap(),
            ProgressEvent::MediaCompleted { .. }
        ));
    }
}
