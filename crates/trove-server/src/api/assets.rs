use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::json;
use trove_core::Error;
use trove_core::model::Asset;
use trove_db::AssetQuery;

use crate::audit;
use crate::auth::Principal;
use crate::content;
use crate::error::ApiResult;
use crate::state::AppState;

#[derive(Serialize)]
pub struct AssetView {
    #[serde(flatten)]
    asset: Asset,
    repository: String,
    format: String,
    download_url: String,
}

#[derive(Deserialize)]
pub struct SearchQuery {
    #[serde(default)]
    q: String,
    repo: Option<String>,
    #[serde(default)]
    limit: i64,
    #[serde(default)]
    offset: i64,
}

pub async fn search(
    State(state): State<AppState>,
    principal: Principal,
    headers: HeaderMap,
    Query(q): Query<SearchQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    principal.require_read(&state)?;
    let repos: HashMap<String, _> = state.db.list_repos().await?.into_iter().map(|r| (r.id.clone(), r)).collect();
    let repo_id = match q.repo.as_deref().filter(|s| !s.is_empty()) {
        Some(name) => {
            Some(repos.values().find(|r| r.name == name).ok_or_else(|| Error::not_found(name.to_string()))?.id.clone())
        }
        None => None,
    };
    let (assets, total) = state
        .db
        .search_assets(&AssetQuery {
            q: q.q,
            repo_id,
            limit: if q.limit == 0 { 50 } else { q.limit },
            offset: q.offset,
        })
        .await?;
    let base = content::base_url(&state, &headers);
    let items: Vec<AssetView> = assets
        .into_iter()
        .filter_map(|a| {
            let r = repos.get(&a.repo_id)?;
            Some(AssetView {
                download_url: format!("{base}/repository/{}/{}", r.name, content::encode_path(&a.path)),
                repository: r.name.clone(),
                format: r.format.to_string(),
                asset: a,
            })
        })
        .collect();
    Ok(Json(json!({ "items": items, "total": total })))
}

pub async fn get_one(
    State(state): State<AppState>,
    principal: Principal,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<AssetView>> {
    principal.require_read(&state)?;
    let asset = state.db.get_asset_by_id(&id).await?.ok_or_else(|| Error::not_found("asset"))?;
    let repo = state.db.get_repo(&asset.repo_id).await?.ok_or_else(|| Error::not_found("repository"))?;
    let base = content::base_url(&state, &headers);
    Ok(Json(AssetView {
        download_url: format!("{base}/repository/{}/{}", repo.name, content::encode_path(&asset.path)),
        repository: repo.name,
        format: repo.format.to_string(),
        asset,
    }))
}

pub async fn remove(
    State(state): State<AppState>,
    principal: Principal,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    principal.require_admin()?;
    let asset = state.db.get_asset_by_id(&id).await?.ok_or_else(|| Error::not_found("asset"))?;
    let repo = state.db.get_repo(&asset.repo_id).await?.ok_or_else(|| Error::not_found("repository"))?;
    state.db.delete_asset(&repo.id, &asset.path).await?;
    audit::record(&state, &principal, &headers, "asset.delete", format!("{}/{}", repo.name, asset.path), "").await;
    Ok(StatusCode::NO_CONTENT)
}
