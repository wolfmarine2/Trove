//! Cargo alternative registry using the sparse index protocol (RFC 2789).
//!
//! Client configuration (`.cargo/config.toml`):
//! ```toml
//! [registries.trove]
//! index = "sparse+https://trove.example.com/repository/cargo-hosted/index/"
//! ```
//!
//! Layout inside a repository:
//! * `index/{prefix}/{name}`                — index file (one JSON line per version)
//! * `index/config.json`                    — cached upstream config (proxy only)
//! * `crates/{name}/{name}-{version}.crate` — crate archives

use axum::Json;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use serde_json::{Map, Value, json};
use trove_core::model::{Asset, RepoKind, Repository, WritePolicy};
use trove_core::util::cargo_index_path;
use trove_core::{Error, Result};

use super::Ctx;
use crate::audit;
use crate::content;
use crate::error::{message_of, status_of};
use crate::proxy;
use crate::state::AppState;

pub fn error_response(e: Error) -> Response {
    let status = status_of(&e);
    let mut resp = (status, Json(json!({ "errors": [{ "detail": message_of(&e) }] }))).into_response();
    if status == StatusCode::UNAUTHORIZED {
        resp.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Cargo login_url=\"/\""));
    }
    resp
}

fn validate_crate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok { Ok(()) } else { Err(Error::bad_request(format!("invalid crate name '{name}'"))) }
}

fn validate_version(v: &str) -> Result<()> {
    let ok =
        !v.is_empty() && v.len() <= 64 && v.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'));
    if ok { Ok(()) } else { Err(Error::bad_request(format!("invalid version '{v}'"))) }
}

fn crate_path(name: &str, version: &str) -> String {
    let n = name.to_ascii_lowercase();
    format!("crates/{n}/{n}-{version}.crate")
}

fn index_asset_path(name: &str) -> String {
    format!("index/{}", cargo_index_path(name))
}

pub async fn handle(mut ctx: Ctx) -> Result<Response> {
    let path = ctx.path.trim_end_matches('/').to_string();
    let segs: Vec<String> = path.split('/').map(str::to_string).collect();
    let segs: Vec<&str> = segs.iter().map(String::as_str).collect();
    match (ctx.method.clone(), segs.as_slice()) {
        (Method::GET | Method::HEAD, ["index", "config.json"]) => config_json(&ctx),
        (Method::GET | Method::HEAD, ["index", rest @ ..]) if !rest.is_empty() => {
            ctx.principal.require_read(&ctx.state)?;
            let name = rest.last().copied().unwrap_or_default();
            validate_crate_name(name)?;
            if rest.join("/") != cargo_index_path(name) {
                return Err(Error::not_found(path.clone()));
            }
            let lines = index_lines(&ctx.state, &ctx.repo, name, 0).await?;
            if lines.is_empty() {
                return Err(Error::not_found(format!("crate '{name}'")));
            }
            let mut body = lines.join("\n");
            body.push('\n');
            Ok(content::serve_bytes("text/plain", body, &ctx.method, &ctx.headers))
        }
        (Method::PUT, ["api", "v1", "crates", "new"]) => publish(&mut ctx).await,
        (Method::GET | Method::HEAD, ["api", "v1", "crates", name, version, "download"]) => {
            ctx.principal.require_read(&ctx.state)?;
            validate_crate_name(name)?;
            validate_version(version)?;
            let asset = download(&ctx.state, &ctx.repo, name, version, 0)
                .await?
                .ok_or_else(|| Error::not_found(format!("{name} {version}")))?;
            content::serve_asset(&ctx.state, &asset, &ctx.method, &ctx.headers).await
        }
        (Method::DELETE, ["api", "v1", "crates", name, version, "yank"]) => set_yanked(&ctx, name, version, true).await,
        (Method::PUT, ["api", "v1", "crates", name, version, "unyank"]) => set_yanked(&ctx, name, version, false).await,
        (Method::GET, ["api", "v1", "crates", _name, "owners"]) => {
            ctx.principal.require_read(&ctx.state)?;
            Ok(Json(json!({ "users": [] })).into_response())
        }
        (Method::PUT | Method::DELETE, ["api", "v1", "crates", _name, "owners"]) => {
            ctx.principal.require_deploy()?;
            Ok(Json(json!({ "ok": true, "msg": "ownership is managed by Trove roles" })).into_response())
        }
        (Method::GET, ["api", "v1", "crates"]) => search(&ctx).await,
        (Method::GET, ["me"]) => {
            Ok(Json(json!({ "message": "create an API token in the Trove UI and run `cargo login`" })).into_response())
        }
        _ => Err(Error::not_found(path)),
    }
}

fn config_json(ctx: &Ctx) -> Result<Response> {
    let base = ctx.repo_url();
    let doc = json!({
        "dl": format!("{base}/api/v1/crates"),
        "api": base,
        "auth-required": !ctx.state.anonymous_read(),
    });
    Ok(content::serve_bytes("application/json", serde_json::to_vec(&doc)?, &ctx.method, &ctx.headers))
}

// ----------------------------------------------------------------- index

fn parse_lines(data: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(data).lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect()
}

fn line_version(line: &str) -> Option<String> {
    serde_json::from_str::<Value>(line).ok()?.get("vers")?.as_str().map(str::to_string)
}

fn index_lines<'a>(
    state: &'a AppState,
    repo: &'a Repository,
    name: &'a str,
    depth: usize,
) -> futures::future::BoxFuture<'a, Result<Vec<String>>> {
    Box::pin(async move {
        if !repo.online {
            return Ok(vec![]);
        }
        let path = index_asset_path(name);
        let asset = match repo.kind {
            RepoKind::Hosted => state.db.get_asset(&repo.id, &path).await?,
            RepoKind::Proxy => {
                let remote = repo.proxy().map(|p| p.remote_url.as_str()).unwrap_or_default();
                let url = content::join_url(remote, &cargo_index_path(name));
                proxy::fetch(state, repo, &path, &url, true).await?
            }
            RepoKind::Group => {
                let mut seen = std::collections::HashSet::new();
                let mut out = Vec::new();
                for m in content::group_members(state, repo, depth).await? {
                    match index_lines(state, &m, name, depth + 1).await {
                        Ok(lines) => {
                            for l in lines {
                                if let Some(v) = line_version(&l)
                                    && seen.insert(v)
                                {
                                    out.push(l);
                                }
                            }
                        }
                        Err(e) => tracing::warn!(member = %m.name, error = %e, "skipping group member"),
                    }
                }
                return Ok(out);
            }
        };
        match asset {
            Some(a) => Ok(parse_lines(&content::read_asset(state, &a).await?)),
            None => Ok(vec![]),
        }
    })
}

async fn upstream_dl(state: &AppState, repo: &Repository) -> Result<String> {
    let remote = repo.proxy().map(|p| p.remote_url.clone()).unwrap_or_default();
    let url = content::join_url(&remote, "config.json");
    let asset = proxy::fetch(state, repo, "index/config.json", &url, true)
        .await?
        .ok_or_else(|| Error::Upstream("upstream has no config.json".into()))?;
    let doc: Value = serde_json::from_slice(&content::read_asset(state, &asset).await?)?;
    doc.get("dl")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| Error::Upstream("upstream config.json has no dl".into()))
}

/// Expand a `dl` template as described in the Cargo registry documentation.
pub fn expand_dl(template: &str, name: &str, version: &str, cksum: &str) -> String {
    let markers = ["{crate}", "{version}", "{prefix}", "{lowerprefix}", "{sha256-checksum}"];
    if !markers.iter().any(|m| template.contains(m)) {
        return format!("{}/{name}/{version}/download", template.trim_end_matches('/'));
    }
    let prefix = {
        let p = cargo_index_path(name);
        p.rsplit_once('/').map(|(a, _)| a.to_string()).unwrap_or_default()
    };
    template
        .replace("{crate}", name)
        .replace("{version}", version)
        .replace("{lowerprefix}", &prefix.to_ascii_lowercase())
        .replace("{prefix}", &prefix)
        .replace("{sha256-checksum}", cksum)
}

fn download<'a>(
    state: &'a AppState,
    repo: &'a Repository,
    name: &'a str,
    version: &'a str,
    depth: usize,
) -> futures::future::BoxFuture<'a, Result<Option<Asset>>> {
    Box::pin(async move {
        if !repo.online {
            return Ok(None);
        }
        let path = crate_path(name, version);
        match repo.kind {
            RepoKind::Hosted => state.db.get_asset(&repo.id, &path).await,
            RepoKind::Proxy => {
                if let Some(a) = state.db.get_asset(&repo.id, &path).await? {
                    return Ok(Some(a));
                }
                let lines = index_lines(state, repo, name, depth).await?;
                let entry = lines
                    .iter()
                    .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                    .find(|v| v.get("vers").and_then(Value::as_str) == Some(version));
                let Some(entry) = entry else { return Ok(None) };
                let cksum = entry.get("cksum").and_then(Value::as_str).unwrap_or_default().to_string();
                let canonical = entry.get("name").and_then(Value::as_str).unwrap_or(name).to_string();
                let dl = upstream_dl(state, repo).await?;
                let url = expand_dl(&dl, &canonical, version, &cksum);
                let asset = proxy::fetch(state, repo, &path, &url, false).await?;
                if let Some(a) = &asset
                    && !cksum.is_empty()
                    && a.blob.sha256 != cksum
                {
                    state.db.delete_asset(&repo.id, &path).await?;
                    return Err(Error::Upstream(format!("checksum mismatch for {name} {version}")));
                }
                Ok(asset)
            }
            RepoKind::Group => {
                let mut failure = None;
                for m in content::group_members(state, repo, depth).await? {
                    match download(state, &m, name, version, depth + 1).await {
                        Ok(Some(a)) => return Ok(Some(a)),
                        Ok(None) => {}
                        Err(e) => failure = Some(content::member_failed(&m, e)),
                    }
                }
                failure.map_or(Ok(None), Err)
            }
        }
    })
}

// ----------------------------------------------------------------- publish

fn read_u32(data: &[u8], at: usize) -> Result<u32> {
    let bytes: [u8; 4] = data
        .get(at..at + 4)
        .and_then(|s| s.try_into().ok())
        .ok_or_else(|| Error::bad_request("truncated publish body"))?;
    Ok(u32::from_le_bytes(bytes))
}

/// Split a publish request body into (metadata JSON, .crate bytes).
pub fn parse_publish_body(body: &[u8]) -> Result<(Value, Bytes)> {
    let json_len = read_u32(body, 0)? as usize;
    let json_bytes = body.get(4..4 + json_len).ok_or_else(|| Error::bad_request("truncated metadata"))?;
    let meta: Value = serde_json::from_slice(json_bytes)?;
    let crate_len = read_u32(body, 4 + json_len)? as usize;
    let start = 8 + json_len;
    let crate_bytes = body.get(start..start + crate_len).ok_or_else(|| Error::bad_request("truncated crate file"))?;
    Ok((meta, Bytes::copy_from_slice(crate_bytes)))
}

/// Build the index line for a publish request.
pub fn index_entry(meta: &Value, cksum: &str) -> Result<Value> {
    let name = meta.get("name").and_then(Value::as_str).ok_or_else(|| Error::bad_request("missing name"))?;
    let vers = meta.get("vers").and_then(Value::as_str).ok_or_else(|| Error::bad_request("missing vers"))?;
    let deps: Vec<Value> = meta
        .get("deps")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|d| {
            let dep_name = d.get("name").cloned().unwrap_or(Value::Null);
            let explicit = d.get("explicit_name_in_toml").filter(|v| !v.is_null()).cloned();
            let mut out = json!({
                "name": explicit.clone().unwrap_or(dep_name.clone()),
                "req": d.get("version_req").cloned().unwrap_or(json!("*")),
                "features": d.get("features").cloned().unwrap_or(json!([])),
                "optional": d.get("optional").cloned().unwrap_or(json!(false)),
                "default_features": d.get("default_features").cloned().unwrap_or(json!(true)),
                "target": d.get("target").cloned().unwrap_or(Value::Null),
                "kind": d.get("kind").cloned().unwrap_or(json!("normal")),
            });
            if let Some(r) = d.get("registry").filter(|v| !v.is_null()) {
                out["registry"] = r.clone();
            }
            if explicit.is_some() {
                out["package"] = dep_name;
            }
            out
        })
        .collect();
    let mut features = Map::new();
    let mut features2 = Map::new();
    if let Some(f) = meta.get("features").and_then(Value::as_object) {
        for (k, v) in f {
            let new_syntax = v
                .as_array()
                .is_some_and(|a| a.iter().filter_map(Value::as_str).any(|s| s.starts_with("dep:") || s.contains("?/")));
            if new_syntax {
                features2.insert(k.clone(), v.clone())
            } else {
                features.insert(k.clone(), v.clone())
            };
        }
    }
    let mut entry = json!({
        "name": name,
        "vers": vers,
        "deps": deps,
        "cksum": cksum,
        "features": features,
        "yanked": false,
        "links": meta.get("links").cloned().unwrap_or(Value::Null),
    });
    if !features2.is_empty() {
        entry["features2"] = Value::Object(features2);
        entry["v"] = json!(2);
    }
    if let Some(rv) = meta.get("rust_version").filter(|v| !v.is_null()) {
        entry["rust_version"] = rv.clone();
    }
    Ok(entry)
}

async fn publish(ctx: &mut Ctx) -> Result<Response> {
    ctx.principal.require_deploy()?;
    if ctx.repo.kind != RepoKind::Hosted {
        return Err(Error::MethodNotAllowed(format!("{} repositories are read-only", ctx.repo.kind)));
    }
    if !ctx.repo.online || ctx.repo.config.write_policy == WritePolicy::ReadOnly {
        return Err(Error::forbidden("repository does not accept publishes"));
    }
    let body = ctx.body_bytes(ctx.state.max_upload() as usize + 16 * 1024 * 1024).await?;
    let (meta, crate_bytes) = parse_publish_body(&body)?;
    let name = meta.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
    let vers = meta.get("vers").and_then(Value::as_str).unwrap_or_default().to_string();
    validate_crate_name(&name)?;
    validate_version(&vers)?;

    let lock = ctx.state.flight_lock(&format!("cargo-index:{}:{}", ctx.repo.id, name.to_ascii_lowercase())).await;
    let _held = lock.lock().await;

    let index_path = index_asset_path(&name);
    let mut lines = match ctx.state.db.get_asset(&ctx.repo.id, &index_path).await? {
        Some(a) => parse_lines(&content::read_asset(&ctx.state, &a).await?),
        None => vec![],
    };
    if let Some(existing) = lines.first().and_then(|l| serde_json::from_str::<Value>(l).ok()) {
        let existing_name = existing.get("name").and_then(Value::as_str).unwrap_or_default();
        if existing_name != name {
            return Err(Error::conflict(format!(
                "crate name '{name}' conflicts with existing crate '{existing_name}'"
            )));
        }
    }
    let allow_redeploy = ctx.repo.config.write_policy == WritePolicy::AllowRedeploy;
    if lines.iter().any(|l| line_version(l).as_deref() == Some(vers.as_str())) {
        if !allow_redeploy {
            return Err(Error::conflict(format!("crate version `{name}@{vers}` is already uploaded")));
        }
        lines.retain(|l| line_version(l).as_deref() != Some(vers.as_str()));
    }

    let stream = futures::stream::iter([Ok::<_, std::io::Error>(crate_bytes)]);
    let attrs = json!({
        "description": meta.get("description").cloned().unwrap_or(Value::Null),
        "license": meta.get("license").cloned().unwrap_or(Value::Null),
        "repository": meta.get("repository").cloned().unwrap_or(Value::Null),
    });
    let opts = content::StoreOpts { content_type: Some("application/gzip"), attributes: attrs, is_metadata: false };
    let asset = content::store(&ctx.state, &ctx.repo, &crate_path(&name, &vers), stream, &ctx.principal, opts).await?;
    let entry = index_entry(&meta, &asset.blob.sha256)?;
    lines.push(serde_json::to_string(&entry)?);
    let mut data = lines.join("\n");
    data.push('\n');
    content::store_bytes(
        &ctx.state,
        &ctx.repo,
        &index_path,
        Bytes::from(data),
        ctx.principal.name(),
        "text/plain",
        json!({}),
    )
    .await?;
    audit::record(
        &ctx.state,
        &ctx.principal,
        &ctx.headers,
        "cargo.publish",
        format!("{}/{name}@{vers}", ctx.repo.name),
        format!("{} bytes", asset.blob.size),
    )
    .await;
    Ok(Json(json!({ "warnings": { "invalid_categories": [], "invalid_badges": [], "other": [] } })).into_response())
}

async fn set_yanked(ctx: &Ctx, name: &str, version: &str, yanked: bool) -> Result<Response> {
    ctx.principal.require_deploy()?;
    if ctx.repo.kind != RepoKind::Hosted {
        return Err(Error::MethodNotAllowed("yank is only supported on hosted repositories".into()));
    }
    validate_crate_name(name)?;
    let lock = ctx.state.flight_lock(&format!("cargo-index:{}:{}", ctx.repo.id, name.to_ascii_lowercase())).await;
    let _held = lock.lock().await;
    let index_path = index_asset_path(name);
    let asset =
        ctx.state.db.get_asset(&ctx.repo.id, &index_path).await?.ok_or_else(|| Error::not_found(name.to_string()))?;
    let mut found = false;
    let lines: Vec<String> = parse_lines(&content::read_asset(&ctx.state, &asset).await?)
        .into_iter()
        .map(|l| match serde_json::from_str::<Value>(&l) {
            Ok(mut v) if v.get("vers").and_then(Value::as_str) == Some(version) => {
                found = true;
                v["yanked"] = json!(yanked);
                serde_json::to_string(&v).unwrap_or(l)
            }
            _ => l,
        })
        .collect();
    if !found {
        return Err(Error::not_found(format!("{name}@{version}")));
    }
    let mut data = lines.join("\n");
    data.push('\n');
    content::store_bytes(
        &ctx.state,
        &ctx.repo,
        &index_path,
        Bytes::from(data),
        ctx.principal.name(),
        "text/plain",
        json!({}),
    )
    .await?;
    let action = if yanked { "cargo.yank" } else { "cargo.unyank" };
    audit::record(&ctx.state, &ctx.principal, &ctx.headers, action, format!("{}/{name}@{version}", ctx.repo.name), "")
        .await;
    Ok(Json(json!({ "ok": true })).into_response())
}

async fn search(ctx: &Ctx) -> Result<Response> {
    ctx.principal.require_read(&ctx.state)?;
    let q = ctx.query_param("q").unwrap_or_default().to_ascii_lowercase();
    let per_page: usize = ctx.query_param("per_page").and_then(|v| v.parse().ok()).unwrap_or(10).min(100);
    let mut repos = vec![ctx.repo.clone()];
    if ctx.repo.kind == RepoKind::Group {
        repos = content::group_members(&ctx.state, &ctx.repo, 0).await?;
    }
    let mut crates = Vec::new();
    let mut total = 0;
    for repo in repos.iter().filter(|r| r.kind == RepoKind::Hosted) {
        for a in ctx.state.db.list_assets_under(&repo.id, "index").await? {
            let Some(name) = a.path.rsplit('/').next() else { continue };
            if name == "config.json" || !name.contains(&q) {
                continue;
            }
            total += 1;
            if crates.len() >= per_page {
                continue;
            }
            let lines = parse_lines(&content::read_asset(&ctx.state, &a).await?);
            let latest = lines
                .iter()
                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                .rfind(|v| !v.get("yanked").and_then(Value::as_bool).unwrap_or(false));
            let Some(latest) = latest else { continue };
            let canonical = latest.get("name").and_then(Value::as_str).unwrap_or(name).to_string();
            let vers = latest.get("vers").and_then(Value::as_str).unwrap_or_default().to_string();
            let description = ctx
                .state
                .db
                .get_asset(&repo.id, &crate_path(&canonical, &vers))
                .await?
                .and_then(|c| c.attributes.get("description").and_then(Value::as_str).map(str::to_string))
                .unwrap_or_default();
            crates.push(json!({ "name": canonical, "max_version": vers, "description": description }));
        }
    }
    Ok(Json(json!({ "crates": crates, "meta": { "total": total } })).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dl_templates() {
        assert_eq!(
            expand_dl("https://static.crates.io/crates", "serde", "1.0.0", "x"),
            "https://static.crates.io/crates/serde/1.0.0/download"
        );
        assert_eq!(
            expand_dl(
                "https://dl/{prefix}/{crate}/{crate}-{version}.crate?c={sha256-checksum}",
                "Serde",
                "1.0.0",
                "ab"
            ),
            "https://dl/se/rd/Serde/Serde-1.0.0.crate?c=ab"
        );
    }

    #[test]
    fn publish_body_and_entry() {
        let meta = json!({
            "name": "demo", "vers": "0.1.0",
            "deps": [
                {"name":"serde","version_req":"^1","features":["derive"],"optional":false,"default_features":true,"target":null,"kind":"normal","registry":"https://github.com/rust-lang/crates.io-index","explicit_name_in_toml":null},
                {"name":"rand","version_req":"^0.8","features":[],"optional":true,"default_features":true,"target":null,"kind":"normal","registry":null,"explicit_name_in_toml":"rnd"}
            ],
            "features": {"default": ["std"], "std": [], "fast": ["dep:rnd"]},
            "links": null, "rust_version": "1.70"
        });
        let json_bytes = serde_json::to_vec(&meta).unwrap();
        let mut body = Vec::new();
        body.extend_from_slice(&(json_bytes.len() as u32).to_le_bytes());
        body.extend_from_slice(&json_bytes);
        body.extend_from_slice(&3u32.to_le_bytes());
        body.extend_from_slice(b"abc");
        let (m, c) = parse_publish_body(&body).unwrap();
        assert_eq!(&c[..], b"abc");
        let e = index_entry(&m, "ck").unwrap();
        assert_eq!(e["deps"][0]["req"], "^1");
        assert_eq!(e["deps"][0]["registry"], "https://github.com/rust-lang/crates.io-index");
        assert_eq!(e["deps"][1]["name"], "rnd");
        assert_eq!(e["deps"][1]["package"], "rand");
        assert!(e["deps"][1].get("registry").is_none());
        assert_eq!(e["features2"]["fast"][0], "dep:rnd");
        assert_eq!(e["v"], 2);
        assert!(e["features"].get("fast").is_none());
        assert!(parse_publish_body(&body[..10]).is_err());
    }
}
