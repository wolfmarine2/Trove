//! Prometheus text exposition at `/metrics`.

use std::fmt::Write;
use std::sync::atomic::Ordering;

use axum::extract::State;
use axum::http::header;
use axum::response::IntoResponse;

use crate::state::AppState;

pub async fn render(State(state): State<AppState>) -> impl IntoResponse {
    let m = &state.metrics;
    let mut out = String::new();
    let mut counter = |name: &str, help: &str, v: u64| {
        let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} counter\n{name} {v}");
    };
    counter("trove_downloads_total", "Assets served to clients.", m.downloads.load(Ordering::Relaxed));
    counter("trove_uploads_total", "Assets written by clients.", m.uploads.load(Ordering::Relaxed));
    counter("trove_proxy_cache_hits_total", "Proxy requests served from cache.", m.proxy_hits.load(Ordering::Relaxed));
    counter(
        "trove_proxy_fetches_total",
        "Requests sent to upstream repositories.",
        m.proxy_fetches.load(Ordering::Relaxed),
    );
    counter(
        "trove_proxy_not_modified_total",
        "Upstream revalidations answered with 304.",
        m.proxy_not_modified.load(Ordering::Relaxed),
    );
    counter("trove_upstream_errors_total", "Failed upstream requests.", m.upstream_errors.load(Ordering::Relaxed));
    counter("trove_auth_failures_total", "Rejected credentials.", m.auth_failures.load(Ordering::Relaxed));
    if let Ok(stats) = state.db.repo_stats().await {
        let names: std::collections::HashMap<String, String> =
            state.db.list_repos().await.unwrap_or_default().into_iter().map(|r| (r.id, r.name)).collect();
        let _ = writeln!(
            out,
            "# HELP trove_repository_assets Number of assets per repository.\n# TYPE trove_repository_assets gauge"
        );
        for s in &stats {
            let name = names.get(&s.repo_id).cloned().unwrap_or_default();
            let _ = writeln!(out, "trove_repository_assets{{repository=\"{name}\"}} {}", s.asset_count);
        }
        let _ = writeln!(
            out,
            "# HELP trove_repository_bytes Logical size of assets per repository.\n# TYPE trove_repository_bytes gauge"
        );
        for s in &stats {
            let name = names.get(&s.repo_id).cloned().unwrap_or_default();
            let _ = writeln!(out, "trove_repository_bytes{{repository=\"{name}\"}} {}", s.total_size);
        }
    }
    if let Ok((count, bytes)) = state.db.blob_totals().await {
        let _ = writeln!(
            out,
            "# HELP trove_blob_count Stored blobs.\n# TYPE trove_blob_count gauge\ntrove_blob_count {count}"
        );
        let _ = writeln!(
            out,
            "# HELP trove_blob_bytes Physical bytes in the blob store.\n# TYPE trove_blob_bytes gauge\ntrove_blob_bytes {bytes}"
        );
    }
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], out)
}
