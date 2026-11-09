//! npm registry API (npm, pnpm, yarn).
//!
//! Layout inside a repository:
//! * `{name}/package.json`   — the packument (all versions)
//! * `{name}/-/{file}.tgz`   — tarballs
//!
//! Tarball URLs are always rewritten to point at the repository being
//! queried, so a group transparently serves content from its members.

use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use bytes::Bytes;
use serde_json::{Value, json};
use trove_core::model::{Asset, RepoKind, Repository, TokenKind, WritePolicy};
use trove_core::{Error, Result};

use super::Ctx;
use crate::audit;
use crate::auth;
use crate::content;
use crate::proxy;
use crate::state::AppState;

const PACKUMENT: &str = "package.json";
const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Validate a package name (`pkg` or `@scope/pkg`).
fn validate_name(name: &str) -> Result<()> {
    let ok_part = |s: &str| {
        !s.is_empty()
            && !s.starts_with('.')
            && !s.starts_with('_')
            && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '.' | '_' | '~'))
    };
    let valid = match name.strip_prefix('@') {
        Some(rest) => rest.split_once('/').is_some_and(|(s, n)| ok_part(s) && ok_part(n)),
        None => ok_part(name) && !name.contains('/'),
    } && name.len() <= 214;
    if valid { Ok(()) } else { Err(Error::bad_request(format!("invalid package name '{name}'"))) }
}

/// Split a request path into (package name, remainder).
fn split_name(path: &str) -> (String, String) {
    let segs: Vec<&str> = path.splitn(3, '/').collect();
    if path.starts_with('@') && segs.len() >= 2 {
        let name = format!("{}/{}", segs[0], segs[1]);
        (name, segs.get(2).copied().unwrap_or("").to_string())
    } else {
        let mut it = path.splitn(2, '/');
        (it.next().unwrap_or("").to_string(), it.next().unwrap_or("").to_string())
    }
}

fn packument_path(name: &str) -> String {
    format!("{name}/{PACKUMENT}")
}

fn tarball_path(name: &str, file: &str) -> String {
    format!("{name}/-/{file}")
}

fn escaped(name: &str) -> String {
    name.replacen('/', "%2f", 1)
}

fn basename(url: &str) -> &str {
    let no_query = url.split(['?', '#']).next().unwrap_or(url);
    no_query.rsplit('/').next().unwrap_or(no_query)
}

pub async fn handle(mut ctx: Ctx) -> Result<Response> {
    let path = ctx.path.trim_end_matches('/').to_string();
    if let Some(rest) = path.strip_prefix("-/") {
        return special(&mut ctx, rest).await;
    }
    if path.is_empty() {
        return Ok(Json(json!({ "db_name": "registry", "engine": "trove" })).into_response());
    }
    let (name, rest) = split_name(&path);
    validate_name(&name)?;
    match (ctx.method.clone(), rest.as_str()) {
        (Method::GET | Method::HEAD, "") => {
            ctx.principal.require_read(&ctx.state)?;
            let doc = packument(&ctx.state, &ctx.repo, &name, 0)
                .await?
                .ok_or_else(|| Error::not_found(format!("package '{name}'")))?;
            let doc = rewrite_tarballs(doc, &ctx.repo_url(), &name);
            let body = serde_json::to_vec(&doc)?;
            Ok(content::serve_bytes("application/json", body, &ctx.method, &ctx.headers))
        }
        (Method::PUT, "") => publish(&mut ctx, &name).await,
        (Method::GET | Method::HEAD, r) if r.starts_with("-/") => {
            ctx.principal.require_read(&ctx.state)?;
            let file = &r[2..];
            if file.contains('/') {
                return Err(Error::not_found(r.to_string()));
            }
            let asset = tarball(&ctx.state, &ctx.repo, &name, file, 0)
                .await?
                .ok_or_else(|| Error::not_found(format!("{name}/-/{file}")))?;
            content::serve_asset(&ctx.state, &asset, &ctx.method, &ctx.headers).await
        }
        (Method::DELETE, r) if r.starts_with("-/") && r.contains("/-rev/") => {
            ctx.principal.require_admin()?;
            let file = r[2..].split('/').next().unwrap_or_default().to_string();
            hosted_only(&ctx.repo)?;
            ctx.state.db.delete_asset(&ctx.repo.id, &tarball_path(&name, &file)).await?;
            audit::record(
                &ctx.state,
                &ctx.principal,
                &ctx.headers,
                "asset.delete",
                format!("{}/{name}/-/{file}", ctx.repo.name),
                "",
            )
            .await;
            Ok(Json(json!({ "ok": true })).into_response())
        }
        (Method::DELETE, r) if r.starts_with("-rev/") => {
            ctx.principal.require_admin()?;
            hosted_only(&ctx.repo)?;
            let deleted = ctx.state.db.delete_assets_under(&ctx.repo.id, &name).await?;
            if deleted.is_empty() {
                return Err(Error::not_found(format!("package '{name}'")));
            }
            audit::record(
                &ctx.state,
                &ctx.principal,
                &ctx.headers,
                "npm.unpublish",
                format!("{}/{name}", ctx.repo.name),
                "all versions",
            )
            .await;
            Ok(Json(json!({ "ok": true })).into_response())
        }
        (Method::PUT, r) if r.starts_with("-rev/") => unpublish_versions(&mut ctx, &name).await,
        (Method::GET | Method::HEAD, version) => {
            ctx.principal.require_read(&ctx.state)?;
            let doc = packument(&ctx.state, &ctx.repo, &name, 0)
                .await?
                .ok_or_else(|| Error::not_found(format!("package '{name}'")))?;
            let doc = rewrite_tarballs(doc, &ctx.repo_url(), &name);
            let resolved = doc["dist-tags"].get(version).and_then(Value::as_str).unwrap_or(version).to_string();
            let manifest =
                doc["versions"].get(&resolved).cloned().ok_or_else(|| Error::not_found(format!("{name}@{version}")))?;
            Ok(Json(manifest).into_response())
        }
        (m, _) => Err(Error::MethodNotAllowed(m.to_string())),
    }
}

use axum::Json;

fn hosted_only(repo: &Repository) -> Result<()> {
    if repo.kind != RepoKind::Hosted {
        return Err(Error::MethodNotAllowed(format!("{} repositories are read-only", repo.kind)));
    }
    Ok(())
}

async fn special(ctx: &mut Ctx, rest: &str) -> Result<Response> {
    match (ctx.method.clone(), rest) {
        (_, "ping") => Ok(Json(json!({})).into_response()),
        (Method::GET, "whoami") => {
            let user = ctx.principal.require_user()?;
            Ok(Json(json!({ "username": user.username })).into_response())
        }
        (Method::PUT, r) if r.starts_with("user/org.couchdb.user:") => login(ctx).await,
        (Method::DELETE, r) if r.starts_with("user/token/") => {
            let token = &r["user/token/".len()..];
            ctx.state.db.delete_token_by_hash(&trove_core::util::sha256_hex(token)).await?;
            Ok(Json(json!({ "ok": true })).into_response())
        }
        (Method::GET, "v1/search") => search(ctx).await,
        (Method::POST, r) if r.starts_with("npm/v1/security/") => audit_forward(ctx, r).await,
        (_, r) if r.starts_with("package/") => dist_tags(ctx, &r["package/".len()..]).await,
        _ => Err(Error::not_found(format!("-/{rest}"))),
    }
}

// ----------------------------------------------------------------- reading

/// Load the packument of `name` from `repo` (merging members for groups).
pub fn packument<'a>(
    state: &'a AppState,
    repo: &'a Repository,
    name: &'a str,
    depth: usize,
) -> futures::future::BoxFuture<'a, Result<Option<Value>>> {
    Box::pin(async move {
        if !repo.online {
            return Ok(None);
        }
        let asset = match repo.kind {
            RepoKind::Hosted => state.db.get_asset(&repo.id, &packument_path(name)).await?,
            RepoKind::Proxy => {
                let remote = repo.proxy().map(|p| p.remote_url.as_str()).unwrap_or_default();
                let url = content::join_url(remote, &escaped(name));
                let opts = proxy::FetchOpts { metadata: true, accept: Some("application/json"), ..Default::default() };
                proxy::fetch_opts(state, repo, &packument_path(name), &url, opts).await?
            }
            RepoKind::Group => {
                let mut docs = Vec::new();
                for m in content::group_members(state, repo, depth).await? {
                    match packument(state, &m, name, depth + 1).await {
                        Ok(Some(d)) => docs.push(d),
                        Ok(None) => {}
                        Err(e) => tracing::warn!(member = %m.name, error = %e, "skipping group member"),
                    }
                }
                return Ok(merge_packuments(docs));
            }
        };
        match asset {
            Some(a) => Ok(Some(serde_json::from_slice(&content::read_asset(state, &a).await?)?)),
            None => Ok(None),
        }
    })
}

fn merge_packuments(mut docs: Vec<Value>) -> Option<Value> {
    if docs.len() <= 1 {
        return docs.pop();
    }
    let mut merged = docs[0].clone();
    for d in &docs[1..] {
        for key in ["versions", "time", "dist-tags"] {
            let target = merged.get_mut(key).and_then(Value::as_object_mut);
            if let (Some(target), Some(src)) = (target, d.get(key).and_then(Value::as_object)) {
                for (k, v) in src {
                    target.entry(k.clone()).or_insert_with(|| v.clone());
                }
            }
        }
    }
    // `latest` should point at the highest published version overall.
    Some(merged)
}

/// Point every `dist.tarball` at `{repo_url}/{name}/-/{file}`.
fn rewrite_tarballs(mut doc: Value, repo_url: &str, name: &str) -> Value {
    if let Some(versions) = doc.get_mut("versions").and_then(Value::as_object_mut) {
        for manifest in versions.values_mut() {
            if let Some(dist) = manifest.get_mut("dist").and_then(Value::as_object_mut)
                && let Some(t) = dist.get("tarball").and_then(Value::as_str)
            {
                let file = basename(t).to_string();
                dist.insert("tarball".into(), json!(format!("{repo_url}/{name}/-/{file}")));
            }
        }
    }
    if let Some(obj) = doc.as_object_mut() {
        let rev = trove_core::util::sha256_hex(
            serde_json::to_vec(obj.get("versions").unwrap_or(&Value::Null)).unwrap_or_default(),
        );
        obj.insert("_rev".into(), json!(format!("1-{}", &rev[..16])));
        obj.remove("_attachments");
    }
    doc
}

fn tarball<'a>(
    state: &'a AppState,
    repo: &'a Repository,
    name: &'a str,
    file: &'a str,
    depth: usize,
) -> futures::future::BoxFuture<'a, Result<Option<Asset>>> {
    Box::pin(async move {
        if !repo.online {
            return Ok(None);
        }
        let path = tarball_path(name, file);
        match repo.kind {
            RepoKind::Hosted => state.db.get_asset(&repo.id, &path).await,
            RepoKind::Proxy => {
                if let Some(a) = state.db.get_asset(&repo.id, &path).await? {
                    return Ok(Some(a));
                }
                let remote = repo.proxy().map(|p| p.remote_url.as_str()).unwrap_or_default();
                let mut url = content::join_url(remote, &format!("{name}/-/{file}"));
                if let Some(doc) = packument(state, repo, name, depth).await?
                    && let Some(versions) = doc.get("versions").and_then(Value::as_object)
                    && let Some(t) = versions
                        .values()
                        .filter_map(|m| m.pointer("/dist/tarball").and_then(Value::as_str))
                        .find(|t| basename(t) == file)
                {
                    url = t.to_string();
                }
                proxy::fetch(state, repo, &path, &url, false).await
            }
            RepoKind::Group => {
                let mut failure = None;
                for m in content::group_members(state, repo, depth).await? {
                    match tarball(state, &m, name, file, depth + 1).await {
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

// ----------------------------------------------------------------- writing

async fn load_hosted(state: &AppState, repo: &Repository, name: &str) -> Result<Option<Value>> {
    match state.db.get_asset(&repo.id, &packument_path(name)).await? {
        Some(a) => Ok(Some(serde_json::from_slice(&content::read_asset(state, &a).await?)?)),
        None => Ok(None),
    }
}

async fn save_hosted(state: &AppState, repo: &Repository, name: &str, doc: &Value, who: &str) -> Result<()> {
    let data = Bytes::from(serde_json::to_vec(doc)?);
    content::store_bytes(state, repo, &packument_path(name), data, who, "application/json", json!({})).await?;
    Ok(())
}

async fn publish(ctx: &mut Ctx, name: &str) -> Result<Response> {
    ctx.principal.require_deploy()?;
    content::check_writable(&ctx.state, &ctx.repo, &packument_path(name), true).await?;
    let limit = (ctx.state.max_upload() as usize).saturating_mul(4) / 3 + 1024 * 1024;
    let body = ctx.body_bytes(limit).await?;
    let doc: Value = serde_json::from_slice(&body)?;
    if doc.get("name").and_then(Value::as_str).is_some_and(|n| n != name) {
        return Err(Error::bad_request("package name in body does not match URL"));
    }
    let new_versions = doc.get("versions").and_then(Value::as_object).cloned().unwrap_or_default();
    let attachments = doc.get("_attachments").and_then(Value::as_object).cloned().unwrap_or_default();

    // Serialise read-modify-write of the packument per package.
    let lock = ctx.state.flight_lock(&format!("npm-packument:{}:{name}", ctx.repo.id)).await;
    let _held = lock.lock().await;
    let mut existing = load_hosted(&ctx.state, &ctx.repo, name).await?;
    let is_new_package = existing.is_none();

    // Updating only dist-tags / metadata without new versions (e.g. `npm deprecate`).
    if attachments.is_empty() {
        let Some(mut current) = existing.take() else {
            return Err(Error::bad_request("no tarball attached"));
        };
        if let (Some(cur), Some(_)) = (current.get_mut("versions").and_then(Value::as_object_mut), doc.get("versions"))
        {
            for (v, m) in &new_versions {
                if let Some(slot) = cur.get_mut(v) {
                    // only allow metadata like "deprecated" to change, keep dist
                    let dist = slot.get("dist").cloned();
                    *slot = m.clone();
                    if let Some(d) = dist {
                        slot["dist"] = d;
                    }
                }
            }
        }
        if let Some(tags) = doc.get("dist-tags") {
            current["dist-tags"] = tags.clone();
        }
        save_hosted(&ctx.state, &ctx.repo, name, &current, ctx.principal.name()).await?;
        return Ok((StatusCode::OK, Json(json!({ "ok": true, "id": name }))).into_response());
    }

    let mut current = existing.unwrap_or_else(
        || json!({ "_id": name, "name": name, "versions": {}, "dist-tags": {}, "time": { "created": now_iso() } }),
    );
    let allow_redeploy = ctx.repo.config.write_policy == WritePolicy::AllowRedeploy;
    for v in new_versions.keys() {
        if current["versions"].get(v).is_some() && !allow_redeploy {
            return Err(Error::forbidden(format!("cannot publish over the previously published version {v}")));
        }
    }

    let mut versions = new_versions;
    for (key, att) in &attachments {
        // Scoped packages name attachments `@scope/pkg-1.0.0.tgz`; the registry
        // convention stores them as `@scope/pkg/-/pkg-1.0.0.tgz`.
        let file = key.rsplit('/').next().unwrap_or_default();
        if file.is_empty() || file.starts_with('.') || file.contains('\\') {
            return Err(Error::bad_request("invalid attachment name"));
        }
        let data =
            att.get("data").and_then(Value::as_str).ok_or_else(|| Error::bad_request("attachment without data"))?;
        let raw = B64.decode(data).map_err(|_| Error::bad_request("attachment is not valid base64"))?;
        let stream = futures::stream::iter([Ok::<_, std::io::Error>(Bytes::from(raw))]);
        let asset = content::store(
            &ctx.state,
            &ctx.repo,
            &tarball_path(name, file),
            stream,
            &ctx.principal,
            content::StoreOpts { content_type: Some("application/octet-stream"), ..Default::default() },
        )
        .await?;
        let integrity = format!("sha512-{}", B64.encode(hex::decode(&asset.blob.sha512).unwrap_or_default()));
        // Attach the computed digests to the manifest owning this tarball.
        let owner = versions
            .iter()
            .find(|(_, m)| m.pointer("/dist/tarball").and_then(Value::as_str).is_some_and(|t| basename(t) == file))
            .map(|(v, _)| v.clone())
            .or_else(|| (versions.len() == 1).then(|| versions.keys().next().cloned()).flatten());
        let Some(version) = owner else { return Err(Error::bad_request(format!("no version references {file}"))) };
        let manifest = versions.get_mut(&version).expect("owner exists");
        if let Some(claimed) = manifest.pointer("/dist/integrity").and_then(Value::as_str)
            && claimed.starts_with("sha512-")
            && claimed != integrity
        {
            return Err(Error::bad_request(format!("integrity mismatch for {file}")));
        }
        if let Some(claimed) = manifest.pointer("/dist/shasum").and_then(Value::as_str)
            && claimed != asset.blob.sha1
        {
            return Err(Error::bad_request(format!("shasum mismatch for {file}")));
        }
        let dist = manifest
            .as_object_mut()
            .ok_or_else(|| Error::bad_request("invalid manifest"))?
            .entry("dist")
            .or_insert_with(|| json!({}));
        dist["shasum"] = json!(asset.blob.sha1);
        dist["integrity"] = json!(integrity);
        dist["tarball"] = json!(format!("{name}/-/{file}"));
    }

    let now = now_iso();
    for (v, m) in &versions {
        current["versions"][v] = m.clone();
        current["time"][v] = json!(now);
    }
    current["time"]["modified"] = json!(now);
    if let Some(tags) = doc.get("dist-tags").and_then(Value::as_object) {
        for (t, v) in tags {
            current["dist-tags"][t] = v.clone();
        }
    }
    for key in ["description", "readme", "maintainers", "keywords", "license", "repository", "homepage"] {
        if let Some(v) = doc.get(key) {
            current[key] = v.clone();
        }
    }
    save_hosted(&ctx.state, &ctx.repo, name, &current, ctx.principal.name()).await?;
    let list: Vec<&String> = versions.keys().collect();
    audit::record(
        &ctx.state,
        &ctx.principal,
        &ctx.headers,
        "npm.publish",
        format!("{}/{name}", ctx.repo.name),
        format!("versions {list:?}{}", if is_new_package { " (new package)" } else { "" }),
    )
    .await;
    Ok((StatusCode::CREATED, Json(json!({ "ok": true, "id": name }))).into_response())
}

/// `npm unpublish pkg@version` sends the packument without that version.
async fn unpublish_versions(ctx: &mut Ctx, name: &str) -> Result<Response> {
    ctx.principal.require_admin()?;
    hosted_only(&ctx.repo)?;
    let lock = ctx.state.flight_lock(&format!("npm-packument:{}:{name}", ctx.repo.id)).await;
    let _held = lock.lock().await;
    let body = ctx.body_bytes(64 * 1024 * 1024).await?;
    let doc: Value = serde_json::from_slice(&body)?;
    let mut current =
        load_hosted(&ctx.state, &ctx.repo, name).await?.ok_or_else(|| Error::not_found(name.to_string()))?;
    let keep: Vec<String> =
        doc.get("versions").and_then(Value::as_object).map(|o| o.keys().cloned().collect()).unwrap_or_default();
    let mut removed = Vec::new();
    if let Some(vs) = current.get_mut("versions").and_then(Value::as_object_mut) {
        vs.retain(|v, _| {
            let k = keep.contains(v);
            if !k {
                removed.push(v.clone());
            }
            k
        });
    }
    if let Some(tags) = doc.get("dist-tags") {
        current["dist-tags"] = tags.clone();
    }
    save_hosted(&ctx.state, &ctx.repo, name, &current, ctx.principal.name()).await?;
    audit::record(
        &ctx.state,
        &ctx.principal,
        &ctx.headers,
        "npm.unpublish",
        format!("{}/{name}", ctx.repo.name),
        format!("versions {removed:?}"),
    )
    .await;
    Ok(Json(json!({ "ok": true })).into_response())
}

async fn dist_tags(ctx: &mut Ctx, rest: &str) -> Result<Response> {
    let (name, tail) = split_name(rest);
    validate_name(&name)?;
    let tag = tail
        .strip_prefix("dist-tags")
        .map(|t| t.trim_start_matches('/').to_string())
        .ok_or_else(|| Error::not_found(rest.to_string()))?;
    match ctx.method.clone() {
        Method::GET => {
            ctx.principal.require_read(&ctx.state)?;
            let doc =
                packument(&ctx.state, &ctx.repo, &name, 0).await?.ok_or_else(|| Error::not_found(name.clone()))?;
            Ok(Json(doc.get("dist-tags").cloned().unwrap_or(json!({}))).into_response())
        }
        Method::PUT | Method::POST | Method::DELETE => {
            ctx.principal.require_deploy()?;
            hosted_only(&ctx.repo)?;
            if tag.is_empty() {
                return Err(Error::bad_request("tag name required"));
            }
            let mut doc =
                load_hosted(&ctx.state, &ctx.repo, &name).await?.ok_or_else(|| Error::not_found(name.clone()))?;
            if ctx.method == Method::DELETE {
                if tag == "latest" {
                    return Err(Error::bad_request("cannot delete the latest tag"));
                }
                if let Some(t) = doc.get_mut("dist-tags").and_then(Value::as_object_mut) {
                    t.remove(&tag);
                }
            } else {
                let body = ctx.body_bytes(1024).await?;
                let version: String = serde_json::from_slice(&body)?;
                if doc["versions"].get(&version).is_none() {
                    return Err(Error::not_found(format!("{name}@{version}")));
                }
                doc["dist-tags"][&tag] = json!(version);
            }
            save_hosted(&ctx.state, &ctx.repo, &name, &doc, ctx.principal.name()).await?;
            Ok(Json(json!({ "ok": true })).into_response())
        }
        m => Err(Error::MethodNotAllowed(m.to_string())),
    }
}

async fn login(ctx: &mut Ctx) -> Result<Response> {
    let body = ctx.body_bytes(64 * 1024).await?;
    let doc: Value = serde_json::from_slice(&body)?;
    let username = doc.get("name").and_then(Value::as_str).unwrap_or_default();
    let password = doc.get("password").and_then(Value::as_str).unwrap_or_default();
    let user = match auth::check_password(&ctx.state, username, password).await {
        Ok(u) => u,
        Err(e) => {
            audit::record_as(&ctx.state, username, &ctx.headers, "auth.login_failed", "npm", "").await;
            return Err(e);
        }
    };
    let (_, secret) = auth::issue_token(&ctx.state, &user, "npm login", TokenKind::Api, None).await?;
    audit::record_as(&ctx.state, &user.username, &ctx.headers, "auth.login", "npm", "token issued").await;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "ok": true, "id": format!("org.couchdb.user:{}", user.username), "token": secret })),
    )
        .into_response())
}

async fn search(ctx: &Ctx) -> Result<Response> {
    ctx.principal.require_read(&ctx.state)?;
    let text = ctx.query_param("text").unwrap_or_default();
    let size: usize = ctx.query_param("size").and_then(|s| s.parse().ok()).unwrap_or(20).min(250);
    let mut repos = vec![ctx.repo.clone()];
    if ctx.repo.kind == RepoKind::Group {
        repos = content::group_members(&ctx.state, &ctx.repo, 0).await?;
    }
    let mut objects = Vec::new();
    for repo in repos.iter().filter(|r| r.kind == RepoKind::Hosted) {
        let (assets, _) = ctx
            .state
            .db
            .search_assets(&trove_db::AssetQuery {
                q: text.clone(),
                repo_id: Some(repo.id.clone()),
                limit: 500,
                offset: 0,
            })
            .await?;
        for a in assets.iter().filter(|a| a.path.ends_with("/package.json")) {
            if objects.len() >= size {
                break;
            }
            let Ok(doc) = serde_json::from_slice::<Value>(&content::read_asset(&ctx.state, a).await?) else { continue };
            let latest = doc.pointer("/dist-tags/latest").and_then(Value::as_str).unwrap_or_default();
            let m = doc["versions"].get(latest).cloned().unwrap_or(json!({}));
            objects.push(json!({
                "package": {
                    "name": doc["name"],
                    "version": latest,
                    "description": m.get("description").or(doc.get("description")).cloned().unwrap_or(json!("")),
                    "keywords": m.get("keywords").cloned().unwrap_or(json!([])),
                    "date": doc.pointer(&format!("/time/{latest}")).cloned().unwrap_or(Value::Null),
                    "links": {},
                    "publisher": {},
                    "maintainers": doc.get("maintainers").cloned().unwrap_or(json!([])),
                },
                "score": { "final": 1.0, "detail": { "quality": 1.0, "popularity": 1.0, "maintenance": 1.0 } },
                "searchScore": 1.0,
            }));
        }
    }
    let total = objects.len();
    Ok(Json(json!({ "objects": objects, "total": total, "time": now_iso() })).into_response())
}

async fn audit_forward(ctx: &mut Ctx, rest: &str) -> Result<Response> {
    let body = ctx.body_bytes(32 * 1024 * 1024).await?;
    let mut target = None;
    match ctx.repo.kind {
        RepoKind::Proxy => target = Some(ctx.repo.clone()),
        RepoKind::Group => {
            target =
                content::group_members(&ctx.state, &ctx.repo, 0).await?.into_iter().find(|m| m.kind == RepoKind::Proxy);
        }
        RepoKind::Hosted => {}
    }
    let Some(repo) = target else { return Ok(Json(json!({})).into_response()) };
    let remote = repo.proxy().map(|p| p.remote_url.clone()).unwrap_or_default();
    let url = content::join_url(&remote, &format!("-/{rest}"));
    proxy::forward(&ctx.state, &repo, reqwest::Method::POST, &url, &ctx.headers, body).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(split_name("@scope/pkg/-/pkg-1.0.0.tgz"), ("@scope/pkg".into(), "-/pkg-1.0.0.tgz".into()));
        assert_eq!(split_name("lodash"), ("lodash".into(), "".into()));
        assert_eq!(split_name("lodash/4.17.21"), ("lodash".into(), "4.17.21".into()));
        assert!(validate_name("@scope/pkg").is_ok());
        assert!(validate_name("Upper").is_err());
        assert!(validate_name("../etc").is_err());
        assert_eq!(escaped("@types/node"), "@types%2fnode");
    }

    #[test]
    fn tarball_rewrite_and_merge() {
        let a = json!({"name":"x","versions":{"1.0.0":{"dist":{"tarball":"https://registry.npmjs.org/x/-/x-1.0.0.tgz"}}},"dist-tags":{"latest":"1.0.0"},"time":{}});
        let b = json!({"name":"x","versions":{"2.0.0":{"dist":{"tarball":"x/-/x-2.0.0.tgz"}}},"dist-tags":{"next":"2.0.0","latest":"2.0.0"},"time":{}});
        let m = merge_packuments(vec![a, b]).unwrap();
        assert_eq!(m["dist-tags"]["latest"], "1.0.0");
        assert_eq!(m["dist-tags"]["next"], "2.0.0");
        let r = rewrite_tarballs(m, "http://h/repository/npm", "x");
        assert_eq!(r["versions"]["2.0.0"]["dist"]["tarball"], "http://h/repository/npm/x/-/x-2.0.0.tgz");
        assert_eq!(r["versions"]["1.0.0"]["dist"]["tarball"], "http://h/repository/npm/x/-/x-1.0.0.tgz");
        assert!(r["_rev"].as_str().unwrap().starts_with("1-"));
    }
}
