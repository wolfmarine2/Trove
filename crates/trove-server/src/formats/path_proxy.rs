//! Shared handler for path-addressed proxy formats (APT, YUM): the client
//! path maps 1:1 onto the remote, and only the freshness rules differ.

use axum::http::Method;
use axum::response::Response;
use trove_core::model::{Asset, RepoKind, Repository};
use trove_core::util::normalize_path;
use trove_core::{Error, Result};

use super::Ctx;
use crate::content;
use crate::proxy::{self, FetchOpts};
use crate::state::AppState;

/// How a path should be cached.
pub struct Policy {
    /// Mutable index/metadata (refreshed after the max age) vs immutable content.
    pub metadata: bool,
    /// Path of the signed index this file belongs to: when that index changes,
    /// this cached file is considered stale.
    pub anchor: Option<String>,
}

pub async fn handle(ctx: Ctx, classify: fn(&str) -> Policy) -> Result<Response> {
    let path = normalize_path(&ctx.path)?;
    match ctx.method {
        Method::GET | Method::HEAD => {
            ctx.principal.require_repo_read(&ctx.state, &ctx.repo)?;
            if path.is_empty() || ctx.path.ends_with('/') {
                return Err(Error::not_found("directory listings are not proxied"));
            }
            let asset = fetch(&ctx, &path, classify).await?.ok_or_else(|| Error::not_found(path.clone()))?;
            content::serve_asset(&ctx.state, &asset, &ctx.method, &ctx.headers).await
        }
        // Admins may evict cached files.
        Method::DELETE => super::raw::delete_path(&ctx, &path).await,
        ref other => Err(Error::MethodNotAllowed(format!("{other}: {} proxies are read-only", ctx.repo.format))),
    }
}

async fn fetch(ctx: &Ctx, path: &str, classify: fn(&str) -> Policy) -> Result<Option<Asset>> {
    fetch_from(&ctx.state, &ctx.repo, path, classify).await
}

/// Fetch `path` through a proxy repository, applying the format's cache policy.
pub async fn fetch_from(
    state: &AppState,
    repo: &Repository,
    path: &str,
    classify: fn(&str) -> Policy,
) -> Result<Option<Asset>> {
    if repo.kind != RepoKind::Proxy || !repo.online {
        return Ok(None);
    }
    let policy = classify(path);
    let remote = repo.proxy().map(|p| p.remote_url.clone()).unwrap_or_default();
    let mut stale_before = None;
    if let Some(anchor) = policy.anchor.as_deref().filter(|a| *a != path) {
        // Refresh the anchor first; if its content changed after this file was
        // cached, the cached copy may no longer match the published hashes.
        let anchor_url = content::join_url(&remote, &content::encode_path(anchor));
        let opts = FetchOpts { metadata: true, ..Default::default() };
        if let Ok(Some(a)) = proxy::fetch_opts(state, repo, anchor, &anchor_url, opts).await {
            stale_before = Some(a.updated_at);
        }
    }
    let url = content::join_url(&remote, &content::encode_path(path));
    let opts = FetchOpts { metadata: policy.metadata, stale_before, ..Default::default() };
    proxy::fetch_opts(state, repo, path, &url, opts).await
}
