//! Format-independent content plumbing: storing, resolving across
//! hosted/proxy/group repositories and serving assets.

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures::Stream;
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use serde_json::Value;
use trove_core::model::{Asset, RepoKind, Repository, WritePolicy};
use trove_core::util::now_millis;
use trove_core::{Error, Result};
use trove_db::NewAsset;

use crate::auth::Principal;
use crate::proxy;
use crate::state::{AppState, Metrics};

/// Maximum nesting of groups inside groups.
pub const MAX_GROUP_DEPTH: usize = 4;
/// Upper bound for metadata documents read into memory.
pub const METADATA_LIMIT: usize = 256 * 1024 * 1024;

const PATH_SEGMENT: &AsciiSet =
    &CONTROLS.add(b' ').add(b'"').add(b'#').add(b'<').add(b'>').add(b'?').add(b'`').add(b'{').add(b'}').add(b'%');

/// Percent-encode each segment of a slash-separated path.
pub fn encode_path(path: &str) -> String {
    path.split('/').map(|s| utf8_percent_encode(s, PATH_SEGMENT).to_string()).collect::<Vec<_>>().join("/")
}

pub fn join_url(base: &str, path: &str) -> String {
    format!("{}/{}", base.trim_end_matches('/'), path.trim_start_matches('/'))
}

/// External base URL (`scheme://host[:port]`) of this server.
pub fn base_url(state: &AppState, headers: &HeaderMap) -> String {
    if !state.cfg.server.base_url.is_empty() {
        return state.cfg.server.base_url.trim_end_matches('/').to_string();
    }
    let h = |name: &str| {
        headers.get(name).and_then(|v| v.to_str().ok()).map(|s| s.split(',').next().unwrap_or("").trim().to_string())
    };
    let proto = h("x-forwarded-proto").filter(|s| !s.is_empty()).unwrap_or_else(|| "http".into());
    let host = h("x-forwarded-host")
        .filter(|s| !s.is_empty())
        .or_else(|| h("host"))
        .unwrap_or_else(|| state.cfg.server.bind.clone());
    format!("{proto}://{host}")
}

/// External URL under which repositories are served (no trailing slash).
pub fn repository_base(state: &AppState, headers: &HeaderMap) -> String {
    let configured = state.cfg.server.repository_base_url.trim().trim_end_matches('/');
    if configured.is_empty() { format!("{}/repository", base_url(state, headers)) } else { configured.to_string() }
}

pub fn repo_url(state: &AppState, headers: &HeaderMap, repo: &Repository) -> String {
    format!("{}/{}", repository_base(state, headers), repo.name)
}

pub fn guess_content_type(path: &str) -> String {
    let lower = path.to_ascii_lowercase();
    let special = [
        (".pom", "application/xml"),
        (".jar", "application/java-archive"),
        (".tgz", "application/gzip"),
        (".crate", "application/gzip"),
        (".whl", "application/zip"),
        (".sha1", "text/plain"),
        (".sha256", "text/plain"),
        (".sha512", "text/plain"),
        (".md5", "text/plain"),
        (".asc", "text/plain"),
    ];
    for (ext, ct) in special {
        if lower.ends_with(ext) {
            return ct.to_string();
        }
    }
    mime_guess::from_path(path).first_or_octet_stream().essence_str().to_string()
}

/// Options for [`store`].
pub struct StoreOpts<'a> {
    pub content_type: Option<&'a str>,
    pub attributes: Value,
    /// Metadata documents may always be rewritten regardless of write policy.
    pub is_metadata: bool,
}

impl Default for StoreOpts<'_> {
    fn default() -> Self {
        Self { content_type: None, attributes: Value::Object(Default::default()), is_metadata: false }
    }
}

/// Enforce online state and write policy before accepting an upload.
pub async fn check_writable(state: &AppState, repo: &Repository, path: &str, is_metadata: bool) -> Result<()> {
    if repo.kind != RepoKind::Hosted {
        return Err(Error::MethodNotAllowed(format!("{} repositories are read-only", repo.kind)));
    }
    if !repo.online {
        return Err(Error::forbidden(format!("repository '{}' is offline", repo.name)));
    }
    match repo.config.write_policy {
        WritePolicy::ReadOnly => Err(Error::forbidden(format!("repository '{}' is read-only", repo.name))),
        WritePolicy::AllowOnce if !is_metadata => {
            if state.db.get_asset(&repo.id, path).await?.is_some() {
                Err(Error::conflict(format!("'{path}' already exists and redeploy is disabled")))
            } else {
                Ok(())
            }
        }
        _ => Ok(()),
    }
}

/// Stream content into a hosted repository (no permission checks here).
pub async fn store<S, E>(
    state: &AppState,
    repo: &Repository,
    path: &str,
    body: S,
    principal: &Principal,
    opts: StoreOpts<'_>,
) -> Result<Asset>
where
    S: Stream<Item = std::result::Result<Bytes, E>> + Send,
    E: std::fmt::Display,
{
    let _guard = state.gc_lock.read().await;
    let blob = state.blobs.ingest(body, state.max_upload()).await?;
    let ct = opts.content_type.map(str::to_string).unwrap_or_else(|| guess_content_type(path));
    let (asset, _) = state
        .db
        .put_asset(NewAsset {
            repo_id: &repo.id,
            path,
            blob: &blob,
            content_type: &ct,
            uploader: principal.name(),
            attributes: opts.attributes,
        })
        .await?;
    Metrics::inc(&state.metrics.uploads);
    Ok(asset)
}

/// Store an in-memory document (used for generated metadata).
pub async fn store_bytes(
    state: &AppState,
    repo: &Repository,
    path: &str,
    data: Bytes,
    uploader: &str,
    content_type: &str,
    attributes: Value,
) -> Result<Asset> {
    let _guard = state.gc_lock.read().await;
    let blob = state.blobs.ingest_bytes(data).await?;
    let (asset, _) = state
        .db
        .put_asset(NewAsset { repo_id: &repo.id, path, blob: &blob, content_type, uploader, attributes })
        .await?;
    Ok(asset)
}

/// Resolve `path` in `repo` for path-addressed formats (raw, maven):
/// hosted → local lookup, proxy → cached upstream fetch, group → members in order.
pub fn resolve<'a>(
    state: &'a AppState,
    repo: &'a Repository,
    path: &'a str,
    is_metadata: bool,
    depth: usize,
) -> futures::future::BoxFuture<'a, Result<Option<Asset>>> {
    Box::pin(async move {
        if !repo.online {
            return Ok(None);
        }
        match repo.kind {
            RepoKind::Hosted => state.db.get_asset(&repo.id, path).await,
            RepoKind::Proxy => {
                let remote = &repo.proxy().map(|p| p.remote_url.clone()).unwrap_or_default();
                let url = join_url(remote, &encode_path(path));
                proxy::fetch(state, repo, path, &url, is_metadata).await
            }
            RepoKind::Group => {
                let mut failure = None;
                for member in group_members(state, repo, depth).await? {
                    match resolve(state, &member, path, is_metadata, depth + 1).await {
                        Ok(Some(a)) => return Ok(Some(a)),
                        Ok(None) => {}
                        Err(e) => failure = Some(member_failed(&member, e)),
                    }
                }
                failure.map_or(Ok(None), Err)
            }
        }
    })
}

/// Log a failing group member; the group keeps searching the others.
pub fn member_failed(member: &Repository, e: Error) -> Error {
    tracing::warn!(member = %member.name, error = %e, "group member failed; trying the next one");
    e
}

/// Load the member repositories of a group (same format, existing, online).
pub async fn group_members(state: &AppState, repo: &Repository, depth: usize) -> Result<Vec<Repository>> {
    if depth >= MAX_GROUP_DEPTH {
        tracing::warn!(repo = %repo.name, "group nesting too deep; ignoring further members");
        return Ok(vec![]);
    }
    let mut out = Vec::new();
    for name in repo.members() {
        match state.db.get_repo_by_name(name).await? {
            Some(m) if m.format == repo.format && m.online => out.push(m),
            Some(_) => {}
            None => tracing::warn!(group = %repo.name, member = %name, "group member does not exist"),
        }
    }
    Ok(out)
}

/// One entry of a directory-style listing.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Entry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: i64,
    pub updated_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asset: Option<Asset>,
}

/// Collapse a flat list of assets below `prefix` into immediate children.
pub fn children(assets: &[Asset], prefix: &str) -> Vec<Entry> {
    use std::collections::BTreeMap;
    let base = if prefix.is_empty() { String::new() } else { format!("{prefix}/") };
    let mut dirs: BTreeMap<String, (i64, i64)> = BTreeMap::new();
    let mut files: Vec<Entry> = Vec::new();
    for a in assets {
        let Some(rest) = a.path.strip_prefix(&base) else { continue };
        match rest.split_once('/') {
            Some((dir, _)) => {
                let e = dirs.entry(dir.to_string()).or_insert((0, 0));
                e.0 += a.blob.size;
                e.1 = e.1.max(a.updated_at);
            }
            None if !rest.is_empty() => files.push(Entry {
                name: rest.to_string(),
                path: a.path.clone(),
                is_dir: false,
                size: a.blob.size,
                updated_at: a.updated_at,
                asset: Some(a.clone()),
            }),
            None => {}
        }
    }
    let mut out: Vec<Entry> = dirs
        .into_iter()
        .map(|(name, (size, updated_at))| Entry {
            path: format!("{base}{name}"),
            name,
            is_dir: true,
            size,
            updated_at,
            asset: None,
        })
        .collect();
    files.sort_by(|a, b| a.name.cmp(&b.name));
    out.extend(files);
    out
}

/// Read a stored asset fully into memory.
pub async fn read_asset(state: &AppState, asset: &Asset) -> Result<Bytes> {
    state.blobs.read_all(&asset.blob.sha256, METADATA_LIMIT).await
}

fn etag_of(asset: &Asset) -> String {
    format!("\"{}\"", asset.blob.sha256)
}

fn http_date(millis: i64) -> String {
    chrono::DateTime::from_timestamp_millis(millis).unwrap_or_default().format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

fn not_modified(req: &HeaderMap, etag: &str) -> bool {
    req.get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag || t.trim() == "*"))
}

/// Stream an asset to the client with integrity headers.
/// A `Range` header that cannot be satisfied (HTTP 416).
#[derive(Debug, PartialEq, Eq)]
pub struct Unsatisfiable;

/// Parse a single `Range: bytes=…` header into an inclusive (start, end).
/// `Ok(None)` means "serve the whole body".
pub fn parse_range(value: &str, size: u64) -> std::result::Result<Option<(u64, u64)>, Unsatisfiable> {
    let Some(spec) = value.trim().strip_prefix("bytes=") else { return Ok(None) };
    if spec.contains(',') || size == 0 {
        return Ok(None); // multi-range: fall back to a full response
    }
    let (a, b) = spec.split_once('-').ok_or(Unsatisfiable)?;
    let (a, b) = (a.trim(), b.trim());
    let range = if a.is_empty() {
        let n: u64 = b.parse().map_err(|_| Unsatisfiable)?;
        if n == 0 {
            return Err(Unsatisfiable);
        }
        (size.saturating_sub(n), size - 1)
    } else {
        let start: u64 = a.parse().map_err(|_| Unsatisfiable)?;
        let end: u64 = if b.is_empty() { size - 1 } else { b.parse::<u64>().map_err(|_| Unsatisfiable)?.min(size - 1) };
        if start > end || start >= size {
            return Err(Unsatisfiable);
        }
        (start, end)
    };
    Ok(Some(range))
}

fn not_modified_since(req: &HeaderMap, updated_at: i64) -> bool {
    if req.contains_key(header::IF_NONE_MATCH) {
        return false;
    }
    req.get(header::IF_MODIFIED_SINCE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| chrono::DateTime::parse_from_rfc2822(v).ok())
        .is_some_and(|since| updated_at / 1000 <= since.timestamp())
}

/// Stream an asset to the client with integrity headers. Supports
/// conditional requests and single byte ranges (resumable downloads).
pub async fn serve_asset(state: &AppState, asset: &Asset, method: &Method, req: &HeaderMap) -> Result<Response> {
    let etag = etag_of(asset);
    let size = asset.blob.size.max(0) as u64;
    let range = match req.get(header::RANGE).and_then(|v| v.to_str().ok()) {
        // If-Range with a different validator means "send everything".
        Some(_) if req.get(header::IF_RANGE).and_then(|v| v.to_str().ok()).is_some_and(|v| v != etag) => Ok(None),
        Some(v) => parse_range(v, size),
        None => Ok(None),
    };
    let mut resp = if not_modified(req, &etag) || not_modified_since(req, asset.updated_at) {
        StatusCode::NOT_MODIFIED.into_response()
    } else if range.is_err() {
        let mut r = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
        if let Ok(v) = HeaderValue::from_str(&format!("bytes */{size}")) {
            r.headers_mut().insert(header::CONTENT_RANGE, v);
        }
        r
    } else if method == Method::HEAD {
        let mut r = Response::new(Body::empty());
        r.headers_mut().insert(header::CONTENT_LENGTH, HeaderValue::from(size));
        r
    } else {
        let mut r = match range {
            Ok(Some((start, end))) => {
                let stream = state.blobs.open_range(&asset.blob.sha256, start, end + 1).await?;
                let mut r = Response::new(Body::from_stream(stream));
                *r.status_mut() = StatusCode::PARTIAL_CONTENT;
                r.headers_mut().insert(header::CONTENT_LENGTH, HeaderValue::from(end - start + 1));
                if let Ok(v) = HeaderValue::from_str(&format!("bytes {start}-{end}/{size}")) {
                    r.headers_mut().insert(header::CONTENT_RANGE, v);
                }
                r
            }
            _ => {
                let stream = state.blobs.open(&asset.blob.sha256).await?;
                let mut r = Response::new(Body::from_stream(stream));
                r.headers_mut().insert(header::CONTENT_LENGTH, HeaderValue::from(size));
                r
            }
        };
        r.headers_mut().insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
        Metrics::inc(&state.metrics.downloads);
        // Record downloads at most hourly per asset to limit write load.
        if asset.last_downloaded_at.is_none_or(|t| now_millis() - t > 3_600_000) {
            let (db, id) = (state.db.clone(), asset.id.clone());
            tokio::spawn(async move {
                let _ = db.touch_asset(&id).await;
            });
        }
        r
    };
    let h = resp.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&asset.content_type) {
        h.insert(header::CONTENT_TYPE, v);
    }
    h.insert(header::ETAG, HeaderValue::from_str(&etag).expect("hex etag"));
    if let Ok(v) = HeaderValue::from_str(&http_date(asset.updated_at)) {
        h.insert(header::LAST_MODIFIED, v);
    }
    for (name, value) in [
        ("x-checksum-sha1", &asset.blob.sha1),
        ("x-checksum-sha256", &asset.blob.sha256),
        ("x-checksum-md5", &asset.blob.md5),
    ] {
        if let Ok(v) = HeaderValue::from_str(value) {
            h.insert(name, v);
        }
    }
    Ok(resp)
}

/// Serve an in-memory body (generated metadata).
pub fn serve_bytes(content_type: &str, data: impl Into<Bytes>, method: &Method, req: &HeaderMap) -> Response {
    let data: Bytes = data.into();
    let etag = format!("\"{}\"", trove_core::util::sha256_hex(&data));
    if not_modified(req, &etag) {
        let mut r = StatusCode::NOT_MODIFIED.into_response();
        r.headers_mut().insert(header::ETAG, HeaderValue::from_str(&etag).expect("hex etag"));
        return r;
    }
    let len = data.len();
    let body = if method == Method::HEAD { Body::empty() } else { Body::from(data) };
    let mut r = Response::new(body);
    let h = r.headers_mut();
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    if let Ok(v) = HeaderValue::from_str(content_type) {
        h.insert(header::CONTENT_TYPE, v);
    }
    h.insert(header::ETAG, HeaderValue::from_str(&etag).expect("hex etag"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding() {
        assert_eq!(encode_path("a b/c#d/e.jar"), "a%20b/c%23d/e.jar");
        assert_eq!(join_url("https://x/maven2/", "/org/a"), "https://x/maven2/org/a");
    }

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-4", 10), Ok(Some((0, 4))));
        assert_eq!(parse_range("bytes=5-", 10), Ok(Some((5, 9))));
        assert_eq!(parse_range("bytes=-3", 10), Ok(Some((7, 9))));
        assert_eq!(parse_range("bytes=8-100", 10), Ok(Some((8, 9))));
        assert_eq!(parse_range("bytes=10-", 10), Err(Unsatisfiable));
        assert_eq!(parse_range("bytes=0-1,4-5", 10), Ok(None));
        assert_eq!(parse_range("items=0-1", 10), Ok(None));
    }

    #[test]
    fn content_types() {
        assert_eq!(guess_content_type("a/b.pom"), "application/xml");
        assert_eq!(guess_content_type("a/b.json"), "application/json");
        assert_eq!(guess_content_type("a/b.unknownext"), "application/octet-stream");
    }
}
