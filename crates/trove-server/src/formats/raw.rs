//! Raw format: arbitrary files addressed by path (`curl -T file .../path`).

use axum::http::{Method, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use trove_core::model::RepoKind;
use trove_core::util::normalize_path;
use trove_core::{Error, Result};

use super::Ctx;
use crate::audit;
use crate::content::{self, StoreOpts};

pub async fn handle(mut ctx: Ctx) -> Result<Response> {
    let wants_dir = ctx.path.is_empty() || ctx.path.ends_with('/');
    let path = normalize_path(&ctx.path)?;
    match ctx.method.clone() {
        Method::GET | Method::HEAD => {
            ctx.principal.require_read(&ctx.state)?;
            if wants_dir {
                return listing(&ctx, &path).await;
            }
            match content::resolve(&ctx.state, &ctx.repo, &path, false, 0).await? {
                Some(asset) => content::serve_asset(&ctx.state, &asset, &ctx.method, &ctx.headers).await,
                None => Err(Error::not_found(path)),
            }
        }
        Method::PUT => {
            if path.is_empty() || wants_dir {
                return Err(Error::bad_request("a file path is required"));
            }
            ctx.principal.require_deploy()?;
            content::check_writable(&ctx.state, &ctx.repo, &path, false).await?;
            let ct = ctx.headers.get("content-type").and_then(|v| v.to_str().ok()).map(str::to_string);
            let body = ctx.body_stream();
            let opts = StoreOpts { content_type: ct.as_deref(), ..Default::default() };
            let asset = content::store(&ctx.state, &ctx.repo, &path, body, &ctx.principal, opts).await?;
            audit::record(
                &ctx.state,
                &ctx.principal,
                &ctx.headers,
                "asset.upload",
                format!("{}/{}", ctx.repo.name, path),
                format!("{} bytes", asset.blob.size),
            )
            .await;
            Ok(StatusCode::CREATED.into_response())
        }
        Method::DELETE => delete_path(&ctx, &path).await,
        other => Err(Error::MethodNotAllowed(other.to_string())),
    }
}

/// Delete a file or a whole directory from a hosted repository (admin only).
pub async fn delete_path(ctx: &Ctx, path: &str) -> Result<Response> {
    ctx.principal.require_admin()?;
    if ctx.repo.kind == RepoKind::Group {
        return Err(Error::MethodNotAllowed("cannot delete from a group".into()));
    }
    let deleted = if let Some(a) = ctx.state.db.delete_asset(&ctx.repo.id, path).await? {
        vec![a]
    } else {
        ctx.state.db.delete_assets_under(&ctx.repo.id, path).await?
    };
    if deleted.is_empty() {
        return Err(Error::not_found(path.to_string()));
    }
    audit::record(
        &ctx.state,
        &ctx.principal,
        &ctx.headers,
        "asset.delete",
        format!("{}/{}", ctx.repo.name, path),
        format!("{} file(s)", deleted.len()),
    )
    .await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn listing(ctx: &Ctx, prefix: &str) -> Result<Response> {
    if ctx.repo.kind != RepoKind::Hosted && ctx.repo.kind != RepoKind::Proxy {
        return Err(Error::not_found("directory listings are not available for groups"));
    }
    let assets = ctx.state.db.list_assets_under(&ctx.repo.id, prefix).await?;
    let entries = content::children(&assets, prefix);
    if entries.is_empty() && !prefix.is_empty() {
        return Err(Error::not_found(prefix.to_string()));
    }
    let title = format!("/{}/{}", ctx.repo.name, prefix);
    let mut html = format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>{t}</title></head><body><h1>Index of {t}</h1><ul>",
        t = escape(&title)
    );
    if !prefix.is_empty() {
        html.push_str("<li><a href=\"../\">../</a></li>");
    }
    for e in entries {
        let name = if e.is_dir { format!("{}/", e.name) } else { e.name.clone() };
        html.push_str(&format!("<li><a href=\"{}\">{}</a></li>", escape(&content::encode_path(&name)), escape(&name)));
    }
    html.push_str("</ul></body></html>");
    Ok(Html(html).into_response())
}

pub fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}
