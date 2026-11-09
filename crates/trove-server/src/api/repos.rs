use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::json;
use trove_core::model::*;
use trove_core::util::{new_id, normalize_path, now_millis};
use trove_core::{Error, Result};

use crate::audit;
use crate::auth::Principal;
use crate::content;
use crate::error::ApiResult;
use crate::state::AppState;

const MASK: &str = "********";

#[derive(Serialize)]
pub struct RepoView {
    #[serde(flatten)]
    repo: Repository,
    url: String,
    asset_count: i64,
    total_size: i64,
}

fn masked(mut repo: Repository) -> Repository {
    if let Some(p) = repo.config.proxy.as_mut() {
        if !p.password.is_empty() {
            p.password = MASK.into();
        }
        if !p.outbound.password.is_empty() {
            p.outbound.password = MASK.into();
        }
    }
    repo
}

async fn view(
    state: &AppState,
    headers: &HeaderMap,
    repo: Repository,
    stats: &HashMap<String, (i64, i64)>,
) -> RepoView {
    let (asset_count, total_size) = stats.get(&repo.id).copied().unwrap_or((0, 0));
    RepoView { url: content::repo_url(state, headers, &repo), repo: masked(repo), asset_count, total_size }
}

async fn stats_map(state: &AppState) -> Result<HashMap<String, (i64, i64)>> {
    Ok(state.db.repo_stats().await?.into_iter().map(|s| (s.repo_id, (s.asset_count, s.total_size))).collect())
}

pub async fn list(
    State(state): State<AppState>,
    principal: Principal,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<RepoView>>> {
    principal.require_read(&state)?;
    let stats = stats_map(&state).await?;
    let mut out = Vec::new();
    for r in state.db.list_repos().await? {
        out.push(view(&state, &headers, r, &stats).await);
    }
    Ok(Json(out))
}

pub async fn get_one(
    State(state): State<AppState>,
    principal: Principal,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> ApiResult<Json<RepoView>> {
    principal.require_read(&state)?;
    let repo = state.repo(&name).await?;
    let stats = stats_map(&state).await?;
    Ok(Json(view(&state, &headers, repo, &stats).await))
}

#[derive(Deserialize)]
pub struct CreateReq {
    name: String,
    format: Format,
    kind: RepoKind,
    #[serde(default)]
    description: String,
    #[serde(default = "yes")]
    online: bool,
    #[serde(default)]
    config: RepoConfig,
}

fn yes() -> bool {
    true
}

fn normalise_config(format: Format, kind: RepoKind, config: &mut RepoConfig) {
    match kind {
        RepoKind::Hosted => {
            config.proxy = None;
            config.group = None;
        }
        RepoKind::Proxy => {
            config.group = None;
            if let Some(p) = config.proxy.as_mut() {
                if p.metadata_max_age_secs == 0 {
                    p.metadata_max_age_secs = DEFAULT_METADATA_MAX_AGE_SECS;
                }
                p.remote_url = p.remote_url.trim().to_string();
            }
        }
        RepoKind::Group => {
            config.proxy = None;
        }
    }
    if format == Format::Maven {
        config.version_policy.get_or_insert(VersionPolicy::Release);
    } else {
        config.version_policy = None;
    }
}

async fn validate_members(state: &AppState, repo: &Repository) -> Result<()> {
    for m in repo.members() {
        let member = state
            .db
            .get_repo_by_name(m)
            .await?
            .ok_or_else(|| Error::bad_request(format!("group member '{m}' does not exist")))?;
        if member.format != repo.format {
            return Err(Error::bad_request(format!("group member '{m}' is a {} repository", member.format)));
        }
        // Reject direct cycles (A contains B contains A).
        if member.members().iter().any(|n| n == &repo.name) {
            return Err(Error::bad_request(format!("group member '{m}' already contains '{}'", repo.name)));
        }
    }
    Ok(())
}

pub async fn create(
    State(state): State<AppState>,
    principal: Principal,
    headers: HeaderMap,
    Json(mut req): Json<CreateReq>,
) -> ApiResult<(StatusCode, Json<RepoView>)> {
    principal.require_admin()?;
    normalise_config(req.format, req.kind, &mut req.config);
    let now = now_millis();
    let repo = Repository {
        id: new_id(),
        name: req.name.trim().to_string(),
        format: req.format,
        kind: req.kind,
        online: req.online,
        description: req.description,
        config: req.config,
        created_at: now,
        updated_at: now,
    };
    repo.validate()?;
    validate_members(&state, &repo).await?;
    state.db.create_repo(&repo).await?;
    audit::record(
        &state,
        &principal,
        &headers,
        "repository.create",
        &repo.name,
        format!("{} {}", repo.format, repo.kind),
    )
    .await;
    let stats = HashMap::new();
    Ok((StatusCode::CREATED, Json(view(&state, &headers, repo, &stats).await)))
}

#[derive(Deserialize)]
pub struct UpdateReq {
    description: Option<String>,
    online: Option<bool>,
    config: Option<RepoConfig>,
}

pub async fn update(
    State(state): State<AppState>,
    principal: Principal,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(req): Json<UpdateReq>,
) -> ApiResult<Json<RepoView>> {
    principal.require_admin()?;
    let mut repo = state.repo(&name).await?;
    if let Some(d) = req.description {
        repo.description = d;
    }
    if let Some(o) = req.online {
        repo.online = o;
    }
    if let Some(mut cfg) = req.config {
        // Keep the stored upstream password when the masked value is sent back.
        if let (Some(new), Some(old)) = (cfg.proxy.as_mut(), repo.config.proxy.as_ref()) {
            if new.password == MASK {
                new.password = old.password.clone();
            }
            if new.outbound.password == MASK {
                new.outbound.password = old.outbound.password.clone();
            }
            if new.invalidated_at == 0 {
                new.invalidated_at = old.invalidated_at;
            }
        }
        normalise_config(repo.format, repo.kind, &mut cfg);
        repo.config = cfg;
    }
    repo.validate()?;
    validate_members(&state, &repo).await?;
    state.db.update_repo(&repo).await?;
    state.clear_negative_for_repo(&repo.id).await;
    audit::record(&state, &principal, &headers, "repository.update", &repo.name, "").await;
    let stats = stats_map(&state).await?;
    Ok(Json(view(&state, &headers, repo, &stats).await))
}

pub async fn remove(
    State(state): State<AppState>,
    principal: Principal,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    principal.require_admin()?;
    let repo = state.repo(&name).await?;
    let referencing: Vec<String> = state
        .db
        .list_repos()
        .await?
        .into_iter()
        .filter(|r| r.members().iter().any(|m| m == &repo.name))
        .map(|r| r.name)
        .collect();
    if !referencing.is_empty() {
        return Err(Error::conflict(format!("repository is a member of group(s): {}", referencing.join(", "))).into());
    }
    state.db.delete_repo(&repo.id).await?;
    audit::record(&state, &principal, &headers, "repository.delete", &repo.name, "").await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct BrowseQuery {
    #[serde(default)]
    path: String,
}

pub async fn browse(
    State(state): State<AppState>,
    principal: Principal,
    Path(name): Path<String>,
    Query(q): Query<BrowseQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    principal.require_read(&state)?;
    let repo = state.repo(&name).await?;
    let prefix = normalize_path(&q.path)?;
    let assets = state.db.list_assets_under(&repo.id, &prefix).await?;
    let entries = content::children(&assets, &prefix);
    Ok(Json(json!({ "path": prefix, "entries": entries })))
}

pub async fn invalidate_cache(
    State(state): State<AppState>,
    principal: Principal,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    principal.require_admin()?;
    let mut repo = state.repo(&name).await?;
    let Some(p) = repo.config.proxy.as_mut() else {
        return Err(Error::bad_request("only proxy repositories have a cache").into());
    };
    p.invalidated_at = now_millis();
    state.db.update_repo(&repo).await?;
    state.clear_negative_for_repo(&repo.id).await;
    audit::record(&state, &principal, &headers, "repository.invalidate_cache", &repo.name, "").await;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct RemoteTestReq {
    format: Format,
    proxy: ProxyConfig,
    /// Existing repository whose stored secrets replace masked values.
    #[serde(default)]
    name: Option<String>,
}

#[derive(Serialize)]
pub struct RemoteTestResult {
    ok: bool,
    url: String,
    status: Option<u16>,
    elapsed_ms: u64,
    message: String,
}

/// Check that a remote is reachable with the given credentials and outbound
/// proxy, without saving anything.
pub async fn test_remote(
    State(state): State<AppState>,
    principal: Principal,
    Json(mut req): Json<RemoteTestReq>,
) -> ApiResult<Json<RemoteTestResult>> {
    principal.require_admin()?;
    if let Some(name) = req.name.as_deref()
        && let Some(old) = state.db.get_repo_by_name(name).await?.and_then(|r| r.config.proxy)
    {
        if req.proxy.password == MASK {
            req.proxy.password = old.password;
        }
        if req.proxy.outbound.password == MASK {
            req.proxy.outbound.password = old.outbound.password;
        }
    }
    req.proxy.outbound.validate()?;
    let remote = req.proxy.remote_url.trim().trim_end_matches('/').to_string();
    if !(remote.starts_with("http://") || remote.starts_with("https://")) {
        return Err(Error::bad_request("remote URL must start with http:// or https://").into());
    }
    let url = match req.format {
        Format::Docker => format!("{}/v2/", remote.strip_suffix("/v2").unwrap_or(&remote)),
        Format::Cargo => format!("{remote}/config.json"),
        Format::Yum => format!("{remote}/repodata/repomd.xml"),
        _ => format!("{remote}/"),
    };
    let client = state.client_for_outbound(&req.proxy.outbound)?;
    let mut request = client.get(&url);
    if !req.proxy.username.is_empty() && req.format != Format::Docker {
        request = request.basic_auth(&req.proxy.username, Some(&req.proxy.password));
    }
    let started = std::time::Instant::now();
    let result = match request.send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            // 401 (registry challenge) and 404 (no index at the root) still
            // prove the network path to the remote works.
            let reachable = (200..400).contains(&status) || status == 401 || status == 404;
            let message = match status {
                407 => "the outbound proxy rejected the credentials (HTTP 407)".to_string(),
                401 => "reachable; the remote requires authentication (HTTP 401)".to_string(),
                404 => "reachable; nothing at the repository root (HTTP 404)".to_string(),
                403 => "the remote or the proxy denied access (HTTP 403)".to_string(),
                405 => "HTTP 405: the outbound proxy may not support plain-HTTP forwarding; try an https:// remote"
                    .to_string(),
                s => format!("HTTP {s}"),
            };
            RemoteTestResult {
                ok: reachable,
                url,
                status: Some(status),
                elapsed_ms: started.elapsed().as_millis() as u64,
                message,
            }
        }
        Err(e) => {
            let mut message = e.to_string();
            let mut src = std::error::Error::source(&e);
            while let Some(c) = src {
                message.push_str(": ");
                message.push_str(&c.to_string());
                src = c.source();
            }
            RemoteTestResult { ok: false, url, status: None, elapsed_ms: started.elapsed().as_millis() as u64, message }
        }
    };
    Ok(Json(result))
}
