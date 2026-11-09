//! Blob garbage collection.
//!
//! Blobs are content-addressed and shared between assets, so deleting an
//! asset never deletes its blob directly. Instead this task periodically
//! removes blobs that no asset references any more.

use std::time::Duration;

use serde::Serialize;
use trove_core::Result;
use trove_core::util::now_millis;

use crate::state::AppState;

/// Unreferenced blobs younger than this are kept, covering uploads whose
/// metadata write is still in flight on another node.
const GRACE_MS: i64 = 10 * 60 * 1000;

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub deleted_blobs: u64,
    pub freed_bytes: u64,
    pub expired_tokens: u64,
}

pub async fn run(state: &AppState) -> Result<Report> {
    let _exclusive = state.gc_lock.write().await;
    let mut report = Report { expired_tokens: state.db.purge_expired_tokens().await?, ..Default::default() };
    loop {
        let batch = state.db.orphan_blobs(now_millis() - GRACE_MS, 500).await?;
        if batch.is_empty() {
            break;
        }
        let mut progressed = false;
        for sha in batch {
            let size = state.db.blob_size(&sha).await?.unwrap_or(0);
            if state.db.delete_blob_if_orphan(&sha).await? {
                state.blobs.delete(&sha).await?;
                report.deleted_blobs += 1;
                report.freed_bytes += size as u64;
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }
    if report.deleted_blobs > 0 {
        tracing::info!(blobs = report.deleted_blobs, bytes = report.freed_bytes, "garbage collection finished");
    }
    Ok(report)
}

/// Run GC periodically in the background.
pub fn spawn_periodic(state: AppState, every: Duration) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(every);
        ticker.tick().await; // skip the immediate tick
        loop {
            ticker.tick().await;
            if let Err(e) = run(&state).await {
                tracing::error!(error = %e, "garbage collection failed");
            }
        }
    });
}
