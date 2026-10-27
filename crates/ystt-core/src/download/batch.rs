//! Batch download orchestration.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use futures_util::{StreamExt, stream};

use crate::config::DownloadOptions;
use crate::progress::{ProgressEmitter, ProgressStage, next_job_id};

use super::{BatchItemResult, BatchOptions, BatchResult, Ystt, millis};

impl Ystt {
    /// Download a batch of URLs with bounded concurrency.
    ///
    /// Results preserve input order. A failed item does not cancel the rest;
    /// the error is returned as a typed [`ErrorInfo`] in its slot.
    pub async fn download_many<I, S>(&self, urls: I, options: BatchOptions) -> BatchResult
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let started = Instant::now();
        let urls: Vec<String> = urls.into_iter().map(Into::into).collect();
        let total_items = urls.len();
        let job_id = next_job_id();
        let root = ProgressEmitter::batch(options.progress.clone(), job_id, total_items);
        root.emit(ProgressStage::BatchStarted { total_items });
        let completed_items = Arc::new(AtomicUsize::new(0));
        let failed_items = Arc::new(AtomicUsize::new(0));
        let concurrency = if options.concurrency == 0 {
            4
        } else {
            options.concurrency.min(64)
        };
        let mut items = stream::iter(urls.into_iter().enumerate().map(|(index, url)| {
            let downloader = self.clone();
            let batch = options.clone();
            let item_emitter = root.child(&url, index, total_items);
            let batch_emitter = root.clone();
            let completed_items = completed_items.clone();
            let failed_items = failed_items.clone();
            async move {
                let mut item_options = DownloadOptions::new();
                item_options.output_dir = batch.output_dir.clone();
                item_options.quality = batch.quality;
                item_options.filename_template = batch.filename_template.clone();
                item_options.include_audio = batch.include_audio;
                item_options.file_cache = batch.file_cache;
                item_options.metadata_cache = batch.metadata_cache;
                item_options.allow_partial = batch.allow_partial;
                item_options.resume = batch.resume;
                item_options.signal = batch
                    .item_signals
                    .get(&index)
                    .cloned()
                    .or_else(|| batch.signal.clone());
                let result = downloader
                    .download_with_emitter(&url, item_options, &item_emitter)
                    .await;
                let item = match result {
                    Ok(result) => BatchItemResult {
                        url,
                        result: Some(result),
                        error: None,
                    },
                    Err(error) => {
                        item_emitter.emit(ProgressStage::Failed {
                            error: error.info(),
                        });
                        BatchItemResult {
                            url,
                            result: None,
                            error: Some(error.info()),
                        }
                    }
                };
                if item.result.is_some() {
                    let completed = completed_items.fetch_add(1, Ordering::Relaxed) + 1;
                    batch_emitter.emit(ProgressStage::BatchProgress {
                        completed_items: completed,
                        failed_items: failed_items.load(Ordering::Relaxed),
                        total_items,
                    });
                } else {
                    let failed = failed_items.fetch_add(1, Ordering::Relaxed) + 1;
                    batch_emitter.emit(ProgressStage::BatchProgress {
                        completed_items: completed_items.load(Ordering::Relaxed),
                        failed_items: failed,
                        total_items,
                    });
                }
                (index, item)
            }
        }))
        .buffer_unordered(concurrency)
        .collect::<Vec<_>>()
        .await;
        items.sort_by_key(|(index, _)| *index);
        let results: Vec<BatchItemResult> = items.into_iter().map(|(_, item)| item).collect();
        let successes = results.iter().filter(|item| item.result.is_some()).count();
        let failures = results.len().saturating_sub(successes);
        let total_ms = millis(started.elapsed());
        root.emit(ProgressStage::BatchCompleted {
            total_ms,
            successes,
            failures,
        });
        BatchResult {
            items: results,
            total_ms,
        }
    }
}
