//! PyPI: PEP 503 simple index + legacy upload API (pip, uv, poetry, twine).
//!
//! Layout inside a repository:
//! * `packages/{project}/{filename}` — distribution files
//! * `simple/{project}/index.json`  — cached upstream index (proxy only)
//!
//! Index pages are generated per request and always link to the repository
//! being queried, so groups serve files through themselves.

use std::collections::BTreeMap;

use axum::extract::{FromRequest, Multipart};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use trove_core::model::{RepoKind, Repository, WritePolicy};
use trove_core::util::pep503_normalize;
use trove_core::{Error, Result};

use super::Ctx;
use super::raw::escape;
use crate::audit;
use crate::content;
use crate::proxy;
use crate::state::AppState;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DistFile {
    pub filename: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub sha256: String,
    #[serde(default)]
    pub requires_python: String,
    #[serde(default)]
    pub yanked: Option<String>,
}

fn valid_filename(name: &str) -> bool {
    !name.is_empty() && !name.contains('/') && !name.contains('\\') && !name.starts_with('.')
}

pub async fn handle(mut ctx: Ctx) -> Result<Response> {
    let path = ctx.path.clone();
    let trimmed = path.trim_matches('/');
    match ctx.method.clone() {
        Method::POST if trimmed.is_empty() || trimmed == "legacy" => upload(&mut ctx).await,
        Method::GET | Method::HEAD => {
            ctx.principal.require_repo_read(&ctx.state, &ctx.repo)?;
            let segs: Vec<&str> = trimmed.split('/').collect();
            match segs.as_slice() {
                ["simple"] => root_index(&ctx).await,
                ["simple", project] => project_index(&ctx, project).await,
                ["packages", project, filename] => file(&ctx, project, filename).await,
                // Tolerate Nexus-style `packages/{name}/{version}/{file}` links.
                ["packages", project, _version, filename] => file(&ctx, project, filename).await,
                _ => Err(Error::not_found(path)),
            }
        }
        Method::DELETE => {
            let p = trove_core::util::normalize_path(&path)?;
            super::raw::delete_path(&ctx, &p).await
        }
        m => Err(Error::MethodNotAllowed(m.to_string())),
    }
}

// ----------------------------------------------------------------- index

async fn root_index(ctx: &Ctx) -> Result<Response> {
    let mut projects = std::collections::BTreeSet::new();
    let mut repos = vec![ctx.repo.clone()];
    if ctx.repo.kind == RepoKind::Group {
        repos = content::group_members(&ctx.state, &ctx.repo, 0).await?;
    }
    for r in repos {
        for a in ctx.state.db.list_assets_under(&r.id, "packages").await? {
            if let Some(p) = a.path.split('/').nth(1) {
                projects.insert(p.to_string());
            }
        }
    }
    let mut html = String::from(
        "<!DOCTYPE html>\n<html><head><meta name=\"pypi:repository-version\" content=\"1.0\"><title>Simple index</title></head><body>\n",
    );
    for p in projects {
        html.push_str(&format!("<a href=\"{0}/\">{0}</a><br/>\n", escape(&p)));
    }
    html.push_str("</body></html>\n");
    Ok(content::serve_bytes("text/html; charset=utf-8", html, &ctx.method, &ctx.headers))
}

async fn project_index(ctx: &Ctx, project: &str) -> Result<Response> {
    let normalized = pep503_normalize(project);
    if normalized != project {
        // PEP 503: redirect to the normalised name.
        let location = format!("{}/simple/{normalized}/", ctx.repo_url());
        return Ok((StatusCode::MOVED_PERMANENTLY, [("location", location)]).into_response());
    }
    let files = project_files(&ctx.state, &ctx.repo, &normalized, 0).await?;
    if files.is_empty() {
        return Err(Error::not_found(format!("project '{normalized}'")));
    }
    let base = ctx.repo_url();
    let mut html = format!(
        "<!DOCTYPE html>\n<html><head><meta name=\"pypi:repository-version\" content=\"1.0\"><title>Links for {0}</title></head><body>\n<h1>Links for {0}</h1>\n",
        escape(&normalized)
    );
    for f in files {
        let mut href = format!("{base}/packages/{normalized}/{}", content::encode_path(&f.filename));
        if !f.sha256.is_empty() {
            href.push_str(&format!("#sha256={}", f.sha256));
        }
        let mut attrs = String::new();
        if !f.requires_python.is_empty() {
            attrs.push_str(&format!(" data-requires-python=\"{}\"", escape(&f.requires_python)));
        }
        if let Some(reason) = &f.yanked {
            attrs.push_str(&format!(" data-yanked=\"{}\"", escape(reason)));
        }
        html.push_str(&format!("<a href=\"{}\"{attrs}>{}</a><br/>\n", escape(&href), escape(&f.filename)));
    }
    html.push_str("</body></html>\n");
    Ok(content::serve_bytes("text/html; charset=utf-8", html, &ctx.method, &ctx.headers))
}

/// Files available for `project` in `repo`.
pub fn project_files<'a>(
    state: &'a AppState,
    repo: &'a Repository,
    project: &'a str,
    depth: usize,
) -> futures::future::BoxFuture<'a, Result<Vec<DistFile>>> {
    Box::pin(async move {
        if !repo.online {
            return Ok(vec![]);
        }
        match repo.kind {
            RepoKind::Hosted => {
                let assets = state.db.list_assets_under(&repo.id, &format!("packages/{project}")).await?;
                Ok(assets
                    .into_iter()
                    .filter_map(|a| {
                        let filename = a.path.rsplit('/').next()?.to_string();
                        Some(DistFile {
                            filename,
                            url: String::new(),
                            sha256: a.blob.sha256.clone(),
                            requires_python: a
                                .attributes
                                .get("requires_python")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            yanked: a.attributes.get("yanked").and_then(Value::as_str).map(str::to_string),
                        })
                    })
                    .collect())
            }
            RepoKind::Proxy => proxy_index(state, repo, project).await,
            RepoKind::Group => {
                let mut seen = BTreeMap::new();
                for m in content::group_members(state, repo, depth).await? {
                    match project_files(state, &m, project, depth + 1).await {
                        Ok(files) => {
                            for f in files {
                                seen.entry(f.filename.clone()).or_insert(f);
                            }
                        }
                        Err(e) => tracing::warn!(member = %m.name, error = %e, "skipping group member"),
                    }
                }
                Ok(seen.into_values().collect())
            }
        }
    })
}

async fn proxy_index(state: &AppState, repo: &Repository, project: &str) -> Result<Vec<DistFile>> {
    let remote = repo.proxy().map(|p| p.remote_url.as_str()).unwrap_or_default();
    let url = content::join_url(remote, &format!("{project}/"));
    let cache_path = format!("simple/{project}/index.html");
    let opts = proxy::FetchOpts {
        metadata: true,
        accept: Some("application/vnd.pypi.simple.v1+json, text/html;q=0.1"),
        ..Default::default()
    };
    let Some(asset) = proxy::fetch_opts(state, repo, &cache_path, &url, opts).await? else { return Ok(vec![]) };
    let data = content::read_asset(state, &asset).await?;
    let page_url = asset.attributes.get("upstream_url").and_then(Value::as_str).unwrap_or(&url).to_string();
    let parsed = if data.first() == Some(&b'{') {
        parse_json_index(&data)?
    } else {
        parse_html_index(&String::from_utf8_lossy(&data))
    };
    Ok(parsed
        .into_iter()
        .map(|mut f| {
            f.url = url::Url::parse(&page_url).and_then(|b| b.join(&f.url)).map(|u| u.to_string()).unwrap_or(f.url);
            f
        })
        .collect())
}

fn parse_json_index(data: &[u8]) -> Result<Vec<DistFile>> {
    let doc: Value = serde_json::from_slice(data)?;
    let files = doc.get("files").and_then(Value::as_array).cloned().unwrap_or_default();
    Ok(files
        .iter()
        .filter_map(|f| {
            let yanked = match f.get("yanked") {
                Some(Value::Bool(true)) => Some(String::new()),
                Some(Value::String(s)) => Some(s.clone()),
                _ => None,
            };
            Some(DistFile {
                filename: f.get("filename")?.as_str()?.to_string(),
                url: f.get("url")?.as_str()?.to_string(),
                sha256: f.pointer("/hashes/sha256").and_then(Value::as_str).unwrap_or_default().to_string(),
                requires_python: f.get("requires-python").and_then(Value::as_str).unwrap_or_default().to_string(),
                yanked,
            })
        })
        .filter(|f| valid_filename(&f.filename))
        .collect())
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&amp;", "&")
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let needle = format!("{name}=");
    let mut search = 0;
    while let Some(pos) = lower[search..].find(&needle) {
        let idx = search + pos;
        let boundary = idx == 0 || lower.as_bytes()[idx - 1].is_ascii_whitespace();
        let after = &tag[idx + needle.len()..];
        if boundary {
            let value = match after.chars().next() {
                Some(q @ ('"' | '\'')) => after[1..].split(q).next().unwrap_or(""),
                _ => after.split(|c: char| c.is_whitespace() || c == '>').next().unwrap_or(""),
            };
            return Some(unescape(value));
        }
        search = idx + needle.len();
    }
    None
}

/// Parse a PEP 503 HTML page into file entries.
pub fn parse_html_index(html: &str) -> Vec<DistFile> {
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find("<a ").or_else(|| rest.find("<A ")) {
        let Some(end) = rest[start..].find('>') else { break };
        let tag = &rest[start..start + end];
        let after = &rest[start + end + 1..];
        let text_end = after.find("</a>").or_else(|| after.find("</A>")).unwrap_or(after.len());
        let text = unescape(after[..text_end].trim());
        if let Some(href) = attr(tag, "href") {
            let (url, frag) = href
                .split_once('#')
                .map(|(u, f)| (u.to_string(), f.to_string()))
                .unwrap_or((href.clone(), String::new()));
            let sha256 = frag.strip_prefix("sha256=").unwrap_or_default().to_string();
            let filename = if text.is_empty() { url.rsplit('/').next().unwrap_or_default().to_string() } else { text };
            if valid_filename(&filename) {
                out.push(DistFile {
                    filename,
                    url,
                    sha256,
                    requires_python: attr(tag, "data-requires-python").unwrap_or_default(),
                    yanked: attr(tag, "data-yanked"),
                });
            }
        }
        rest = &after[text_end.min(after.len())..];
    }
    out
}

// ----------------------------------------------------------------- files

fn file<'a>(ctx: &'a Ctx, project: &'a str, filename: &'a str) -> futures::future::BoxFuture<'a, Result<Response>> {
    Box::pin(async move {
        if !valid_filename(filename) {
            return Err(Error::bad_request("invalid filename"));
        }
        let project = pep503_normalize(project);
        let asset = find_file(&ctx.state, &ctx.repo, &project, filename, 0)
            .await?
            .ok_or_else(|| Error::not_found(format!("{project}/{filename}")))?;
        content::serve_asset(&ctx.state, &asset, &ctx.method, &ctx.headers).await
    })
}

fn find_file<'a>(
    state: &'a AppState,
    repo: &'a Repository,
    project: &'a str,
    filename: &'a str,
    depth: usize,
) -> futures::future::BoxFuture<'a, Result<Option<trove_core::model::Asset>>> {
    Box::pin(async move {
        if !repo.online {
            return Ok(None);
        }
        let path = format!("packages/{project}/{filename}");
        match repo.kind {
            RepoKind::Hosted => state.db.get_asset(&repo.id, &path).await,
            RepoKind::Proxy => {
                if let Some(a) = state.db.get_asset(&repo.id, &path).await? {
                    return Ok(Some(a));
                }
                let files = proxy_index(state, repo, project).await?;
                let Some(f) = files.into_iter().find(|f| f.filename == filename) else { return Ok(None) };
                let asset = proxy::fetch(state, repo, &path, &f.url, false).await?;
                if let (Some(a), false) = (&asset, f.sha256.is_empty())
                    && a.blob.sha256 != f.sha256
                {
                    state.db.delete_asset(&repo.id, &path).await?;
                    return Err(Error::Upstream(format!("checksum mismatch for {filename}")));
                }
                Ok(asset)
            }
            RepoKind::Group => {
                let mut failure = None;
                for m in content::group_members(state, repo, depth).await? {
                    match find_file(state, &m, project, filename, depth + 1).await {
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

// ----------------------------------------------------------------- upload

async fn upload(ctx: &mut Ctx) -> Result<Response> {
    ctx.principal.require_deploy()?;
    if ctx.repo.kind != RepoKind::Hosted {
        return Err(Error::MethodNotAllowed(format!("{} repositories are read-only", ctx.repo.kind)));
    }
    let mut req = axum::http::Request::builder().method(Method::POST).uri("/");
    for (k, v) in &ctx.headers {
        req = req.header(k, v);
    }
    let mut req = req.body(ctx.take_body()).map_err(|e| Error::bad_request(e.to_string()))?;
    *req.extensions_mut() = ctx.extensions.clone();
    let mut multipart = Multipart::from_request(req, &()).await.map_err(|e| Error::bad_request(e.to_string()))?;

    let mut fields: BTreeMap<String, String> = BTreeMap::new();
    let mut stored: Option<(trove_core::model::Asset, String)> = None;
    while let Some(mut field) = multipart.next_field().await.map_err(|e| Error::bad_request(e.to_string()))? {
        let name = field.name().unwrap_or_default().to_string();
        if name == "content" {
            let filename = field.file_name().unwrap_or_default().to_string();
            if !valid_filename(&filename) {
                return Err(Error::bad_request("invalid or missing filename"));
            }
            let project = fields
                .get("name")
                .map(|n| pep503_normalize(n))
                .ok_or_else(|| Error::bad_request("'name' must be sent before 'content'"))?;
            let path = format!("packages/{project}/{filename}");
            check_exists(ctx, &path).await?;
            let stream = async_stream::stream! {
                loop {
                    match field.chunk().await {
                        Ok(Some(c)) => yield Ok::<Bytes, String>(c),
                        Ok(None) => break,
                        Err(e) => { yield Err(e.to_string()); break; }
                    }
                }
            };
            let attrs = json!({
                "version": fields.get("version").cloned().unwrap_or_default(),
                "requires_python": fields.get("requires_python").cloned().unwrap_or_default(),
                "summary": fields.get("summary").cloned().unwrap_or_default(),
                "filetype": fields.get("filetype").cloned().unwrap_or_default(),
            });
            let opts = content::StoreOpts { content_type: None, attributes: attrs, is_metadata: false };
            let asset = content::store(&ctx.state, &ctx.repo, &path, stream, &ctx.principal, opts).await?;
            stored = Some((asset, path));
        } else {
            let text = field.text().await.map_err(|e| Error::bad_request(e.to_string()))?;
            fields.insert(name, text);
        }
    }
    if fields.get(":action").is_some_and(|a| a != "file_upload") {
        return Err(Error::bad_request("unsupported :action"));
    }
    let Some((asset, path)) = stored else { return Err(Error::bad_request("missing 'content' file")) };
    if let Some(expected) = fields.get("sha256_digest").filter(|s| !s.is_empty())
        && expected.to_ascii_lowercase() != asset.blob.sha256
    {
        ctx.state.db.delete_asset(&ctx.repo.id, &path).await?;
        return Err(Error::bad_request("sha256_digest does not match uploaded content"));
    }
    if let Some(expected) = fields.get("md5_digest").filter(|s| !s.is_empty())
        && expected.to_ascii_lowercase() != asset.blob.md5
    {
        ctx.state.db.delete_asset(&ctx.repo.id, &path).await?;
        return Err(Error::bad_request("md5_digest does not match uploaded content"));
    }
    // Re-attach attributes now that every field is known (field order varies by client).
    let attrs = json!({
        "version": fields.get("version").cloned().unwrap_or_default(),
        "requires_python": fields.get("requires_python").cloned().unwrap_or_default(),
        "summary": fields.get("summary").cloned().unwrap_or_default(),
        "filetype": fields.get("filetype").cloned().unwrap_or_default(),
    });
    ctx.state.db.set_asset_attributes(&asset.id, &attrs).await?;
    audit::record(
        &ctx.state,
        &ctx.principal,
        &ctx.headers,
        "pypi.upload",
        format!("{}/{path}", ctx.repo.name),
        format!("{} bytes", asset.blob.size),
    )
    .await;
    Ok(StatusCode::OK.into_response())
}

async fn check_exists(ctx: &Ctx, path: &str) -> Result<()> {
    if !ctx.repo.online {
        return Err(Error::forbidden("repository is offline"));
    }
    match ctx.repo.config.write_policy {
        WritePolicy::ReadOnly => Err(Error::forbidden("repository is read-only")),
        WritePolicy::AllowRedeploy => Ok(()),
        WritePolicy::AllowOnce => {
            if ctx.state.db.get_asset(&ctx.repo.id, path).await?.is_some() {
                Err(Error::BadRequest("File already exists".into()))
            } else {
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_index_parsing() {
        let html = r#"<!DOCTYPE html><html><body>
            <a href="https://files.example/p/requests-2.31.0-py3-none-any.whl#sha256=abc123" data-requires-python="&gt;=3.7">requests-2.31.0-py3-none-any.whl</a><br/>
            <a href='../../packages/requests/requests-2.0.tar.gz' data-yanked="">requests-2.0.tar.gz</a>
            <a href="/evil/../x">../x</a>
        </body></html>"#;
        let files = parse_html_index(html);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].sha256, "abc123");
        assert_eq!(files[0].requires_python, ">=3.7");
        assert_eq!(files[1].url, "../../packages/requests/requests-2.0.tar.gz");
        assert_eq!(files[1].yanked.as_deref(), Some(""));
    }

    #[test]
    fn json_index_parsing() {
        let doc = br#"{"meta":{"api-version":"1.1"},"name":"six","files":[
            {"filename":"six-1.16.0.tar.gz","url":"https://f/six-1.16.0.tar.gz","hashes":{"sha256":"ff"},"requires-python":">=2.7","yanked":false},
            {"filename":"six-1.0.tar.gz","url":"https://f/six-1.0.tar.gz","hashes":{},"yanked":"broken"}]}"#;
        let files = parse_json_index(doc).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].sha256, "ff");
        assert_eq!(files[1].yanked.as_deref(), Some("broken"));
    }
}
