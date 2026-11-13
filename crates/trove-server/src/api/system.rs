use std::sync::atomic::Ordering;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use serde::Deserialize;
use serde_json::{Value, json};
use trove_db::AuditQuery;

use crate::audit;
use crate::auth::Principal;
use crate::content;
use crate::error::ApiResult;
use crate::gc;
use crate::state::{AppState, SETTING_ANONYMOUS_READ, SETTING_DOCKER_DEFAULT_REPO};

pub async fn status(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "name": "Trove",
        "version": env!("CARGO_PKG_VERSION"),
        "anonymous_read": state.anonymous_read(),
        "uptime_secs": state.started_at.elapsed().as_secs(),
    }))
}

pub async fn stats(State(state): State<AppState>, principal: Principal, headers: HeaderMap) -> ApiResult<Json<Value>> {
    principal.require_read(&state)?;
    let repos = state.db.list_repos().await?;
    let per_repo = state.db.repo_stats().await?;
    let (blob_count, blob_bytes) = state.db.blob_totals().await?;
    let mut by_format = serde_json::Map::new();
    for r in &repos {
        let entry = by_format.entry(r.format.to_string()).or_insert(json!(0));
        *entry = json!(entry.as_i64().unwrap_or(0) + 1);
    }
    let base = content::repository_base(&state, &headers);
    let names: std::collections::HashMap<_, _> =
        repos.iter().filter(|r| principal.can_read_repo(&state, r)).map(|r| (r.id.clone(), r.name.clone())).collect();
    let recent: Vec<Value> = state
        .db
        .recent_assets(15)
        .await?
        .into_iter()
        .filter(|a| a.uploader != "proxy" && names.contains_key(&a.repo_id))
        .map(|a| {
            let repo = names.get(&a.repo_id).cloned().unwrap_or_default();
            json!({
                "id": a.id, "path": a.path, "repository": repo, "size": a.blob.size,
                "updated_at": a.updated_at, "uploader": a.uploader,
                "download_url": format!("{base}/{repo}/{}", content::encode_path(&a.path)),
            })
        })
        .collect();
    let m = &state.metrics;
    Ok(Json(json!({
        "repositories": repos.len(),
        "repositories_by_format": by_format,
        "assets": per_repo.iter().map(|s| s.asset_count).sum::<i64>(),
        "logical_bytes": per_repo.iter().map(|s| s.total_size).sum::<i64>(),
        "blobs": blob_count,
        "stored_bytes": blob_bytes,
        "recent": recent,
        "counters": {
            "downloads": m.downloads.load(Ordering::Relaxed),
            "uploads": m.uploads.load(Ordering::Relaxed),
            "proxy_hits": m.proxy_hits.load(Ordering::Relaxed),
            "proxy_fetches": m.proxy_fetches.load(Ordering::Relaxed),
            "upstream_errors": m.upstream_errors.load(Ordering::Relaxed),
        },
        "storage": state.blobs.description(),
        "database": format!("{:?}", state.db.backend()).to_lowercase(),
    })))
}

pub async fn get_settings(State(state): State<AppState>, principal: Principal) -> ApiResult<Json<Value>> {
    principal.require_admin()?;
    let docker_default = state.db.get_setting(SETTING_DOCKER_DEFAULT_REPO).await?.unwrap_or_default();
    Ok(Json(json!({
        "anonymous_read": state.anonymous_read(),
        "docker_default_repository": docker_default,
        "base_url": state.cfg.server.base_url,
        "max_upload_mb": state.cfg.server.max_upload_mb,
        "session_ttl_hours": state.cfg.security.session_ttl_hours,
        "storage": state.blobs.description(),
        "database": format!("{:?}", state.db.backend()).to_lowercase(),
    })))
}

#[derive(Deserialize)]
pub struct SettingsReq {
    anonymous_read: Option<bool>,
    /// Empty string clears the default.
    docker_default_repository: Option<String>,
}

pub async fn put_settings(
    State(state): State<AppState>,
    principal: Principal,
    headers: HeaderMap,
    Json(req): Json<SettingsReq>,
) -> ApiResult<Json<Value>> {
    principal.require_admin()?;
    if let Some(v) = req.anonymous_read {
        state.db.set_setting(SETTING_ANONYMOUS_READ, if v { "true" } else { "false" }).await?;
        state.anonymous_read.store(v, Ordering::Relaxed);
        audit::record(&state, &principal, &headers, "settings.update", "anonymous_read", v.to_string()).await;
    }
    if let Some(name) = req.docker_default_repository {
        let name = name.trim().to_string();
        if !name.is_empty() {
            let repo = state.repo(&name).await?;
            if repo.format != trove_core::model::Format::Docker {
                return Err(trove_core::Error::bad_request(format!("'{name}' is not a Docker repository")).into());
            }
        }
        state.db.set_setting(SETTING_DOCKER_DEFAULT_REPO, &name).await?;
        audit::record(&state, &principal, &headers, "settings.update", "docker_default_repository", name).await;
    }
    get_settings(State(state), principal).await
}

pub async fn gc(State(state): State<AppState>, principal: Principal, headers: HeaderMap) -> ApiResult<Json<Value>> {
    principal.require_admin()?;
    let report = gc::run(&state).await?;
    audit::record(
        &state,
        &principal,
        &headers,
        "system.gc",
        "blobs",
        format!("{} blobs, {} bytes", report.deleted_blobs, report.freed_bytes),
    )
    .await;
    Ok(Json(json!(report)))
}

#[derive(Deserialize)]
pub struct AuditParams {
    actor: Option<String>,
    action: Option<String>,
    #[serde(default)]
    limit: i64,
    #[serde(default)]
    offset: i64,
}

pub async fn audit(
    State(state): State<AppState>,
    principal: Principal,
    Query(q): Query<AuditParams>,
) -> ApiResult<Json<Value>> {
    principal.require_admin()?;
    let (items, total) =
        state.db.list_audit(&AuditQuery { actor: q.actor, action: q.action, limit: q.limit, offset: q.offset }).await?;
    Ok(Json(json!({ "items": items, "total": total })))
}
