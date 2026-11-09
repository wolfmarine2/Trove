//! Docker / OCI distribution API (`/v2/`), read side: proxy and group
//! repositories, like Nexus' docker-proxy and docker-group.
//!
//! Routing: the Registry API is always rooted at `/v2/`, so the repository is
//! the first path segment of the image name:
//!
//! ```text
//! docker pull trove.example.com/docker-hub/library/nginx:1.27
//!   → GET /v2/docker-hub/library/nginx/manifests/1.27
//! ```
//!
//! For daemons that only accept a bare mirror host (`registry-mirrors`), a
//! *default Docker repository* can be configured: names whose first segment is
//! not a Docker repository are served from it.
//!
//! Layout inside a repository:
//! * `{image}/manifests/sha256:…` — manifests by digest (immutable)
//! * `{image}/tags/{tag}`         — manifest by tag (refreshed after max age)
//! * `{image}/blobs/sha256:…`     — layers and configs (immutable)
//! * `{image}/_tags/list.json`    — cached tag list

use std::collections::BTreeSet;

use axum::Json;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use trove_core::model::{Asset, Format, RepoKind, Repository};
use trove_core::util::now_millis;
use trove_core::{Error, Result};
use trove_db::NewAsset;

use crate::auth;
use crate::content;
use crate::error::{message_of, status_of};
use crate::proxy::{self, FetchOpts};
use crate::state::{AppState, SETTING_DOCKER_DEFAULT_REPO};

const API_VERSION: &str = "registry/2.0";

/// Manifest media types we accept from upstreams, most preferred first.
const MANIFEST_ACCEPT: &str = "application/vnd.oci.image.index.v1+json, \
application/vnd.docker.distribution.manifest.list.v2+json, \
application/vnd.oci.image.manifest.v1+json, \
application/vnd.docker.distribution.manifest.v2+json, \
application/vnd.docker.distribution.manifest.v1+prettyjws;q=0.5, \
application/json;q=0.1";

const DOCKER_HUB_HOSTS: &[&str] = &["registry-1.docker.io", "index.docker.io", "registry.hub.docker.com", "docker.io"];

/// Registry API error with its OCI error code.
struct RegistryError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl RegistryError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self { status, code, message: message.into() }
    }

    fn from_error(e: Error, not_found_code: &'static str) -> Self {
        let status = status_of(&e);
        let code = match &e {
            Error::NotFound(_) => not_found_code,
            Error::Unauthorized => "UNAUTHORIZED",
            Error::Forbidden(_) => "DENIED",
            Error::BadRequest(_) => "NAME_INVALID",
            Error::MethodNotAllowed(_) => "UNSUPPORTED",
            Error::PayloadTooLarge => "SIZE_INVALID",
            Error::Conflict(_) | Error::Upstream(_) | Error::Internal(_) => "UNKNOWN",
        };
        Self { status, code, message: message_of(&e) }
    }
}

impl IntoResponse for RegistryError {
    fn into_response(self) -> Response {
        let body = json!({ "errors": [{ "code": self.code, "message": self.message, "detail": null }] });
        let mut resp = (self.status, Json(body)).into_response();
        if self.status == StatusCode::UNAUTHORIZED {
            resp.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Basic realm=\"Trove\""));
        }
        resp
    }
}

fn with_api_version(mut resp: Response) -> Response {
    resp.headers_mut().insert("docker-distribution-api-version", HeaderValue::from_static(API_VERSION));
    resp
}

/// What a `/v2/...` path asks for.
#[derive(Debug, PartialEq)]
enum Target<'a> {
    Base,
    Catalog,
    Manifest(&'a str, &'a str),
    Blob(&'a str, &'a str),
    Upload(&'a str),
    Tags(&'a str),
}

fn parse_target(path: &str) -> Option<Target<'_>> {
    let path = path.trim_matches('/');
    if path.is_empty() {
        return Some(Target::Base);
    }
    if path == "_catalog" {
        return Some(Target::Catalog);
    }
    if let Some(name) = path.strip_suffix("/tags/list") {
        return Some(Target::Tags(name));
    }
    if let Some(idx) = path.find("/blobs/uploads") {
        return Some(Target::Upload(&path[..idx]));
    }
    if let Some((name, reference)) = path.rsplit_once("/manifests/") {
        return Some(Target::Manifest(name, reference));
    }
    if let Some((name, digest)) = path.rsplit_once("/blobs/") {
        return Some(Target::Blob(name, digest));
    }
    None
}

fn valid_image_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name.split('/').all(|seg| {
            !seg.is_empty()
                && seg.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-'))
                && seg.as_bytes()[0].is_ascii_alphanumeric()
        })
}

fn valid_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag.len() <= 128
        && tag.as_bytes()[0] != b'.'
        && tag.as_bytes()[0] != b'-'
        && tag.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// `sha256:<64 hex>` → hex part.
fn sha256_of(digest: &str) -> Option<&str> {
    let hex = digest.strip_prefix("sha256:")?;
    (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())).then_some(hex)
}

/// Name to request upstream: Docker Hub keeps official images under `library/`.
fn upstream_name(remote: &str, image: &str) -> String {
    let host = url::Url::parse(remote).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
    if DOCKER_HUB_HOSTS.contains(&host.as_str()) && !image.contains('/') {
        format!("library/{image}")
    } else {
        image.to_string()
    }
}

fn remote_base(repo: &Repository) -> String {
    let remote = repo.proxy().map(|p| p.remote_url.trim_end_matches('/').to_string()).unwrap_or_default();
    // Accept both "https://ghcr.io" and "https://ghcr.io/v2".
    remote.strip_suffix("/v2").map(str::to_string).unwrap_or(remote)
}

/// Entry point for `/v2` and `/v2/{*path}`.
pub async fn dispatch(State(state): State<AppState>, req: Request) -> Response {
    let (parts, _body) = req.into_parts();
    let path = parts.uri.path().strip_prefix("/v2").unwrap_or("").to_string();
    let resp = match handle(&state, &parts.method, &parts.headers, &path).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    };
    with_api_version(resp)
}

async fn handle(
    state: &AppState,
    method: &Method,
    headers: &HeaderMap,
    path: &str,
) -> std::result::Result<Response, RegistryError> {
    let principal =
        auth::authenticate(state, headers).await.map_err(|e| RegistryError::from_error(e, "UNAUTHORIZED"))?;
    let target = parse_target(path)
        .ok_or_else(|| RegistryError::new(StatusCode::NOT_FOUND, "NAME_UNKNOWN", "unknown registry path"))?;
    if let Target::Upload(_) = target {
        principal.require_deploy().map_err(|e| RegistryError::from_error(e, "UNSUPPORTED"))?;
        return Err(RegistryError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "UNSUPPORTED",
            "pushing images is not supported yet; Trove serves Docker proxy and group repositories",
        ));
    }
    if !matches!(*method, Method::GET | Method::HEAD) {
        return Err(RegistryError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "UNSUPPORTED",
            "proxy and group repositories are read-only",
        ));
    }
    // `docker login` and clients probing the API need a real 401 to send credentials.
    principal.require_read(state).map_err(|e| RegistryError::from_error(e, "UNAUTHORIZED"))?;

    match target {
        Target::Base => Ok(Json(json!({})).into_response()),
        Target::Catalog => catalog(state, &principal).await.map_err(|e| RegistryError::from_error(e, "NAME_UNKNOWN")),
        Target::Manifest(name, reference) => {
            let (repo, image) = route(state, name).await?;
            if !principal.can_read_repo(state, &repo) {
                return Err(RegistryError::new(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", "authentication required"));
            }
            let asset = manifest(state, &repo, &image, reference, 0)
                .await
                .map_err(|e| RegistryError::from_error(e, "MANIFEST_UNKNOWN"))?
                .ok_or_else(|| {
                    RegistryError::new(
                        StatusCode::NOT_FOUND,
                        "MANIFEST_UNKNOWN",
                        format!("manifest unknown: {image}:{reference}"),
                    )
                })?;
            serve_manifest(state, &asset, method, headers).await
        }
        Target::Blob(name, digest) => {
            let (repo, image) = route(state, name).await?;
            if !principal.can_read_repo(state, &repo) {
                return Err(RegistryError::new(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", "authentication required"));
            }
            let hex = sha256_of(digest).ok_or_else(|| {
                RegistryError::new(StatusCode::BAD_REQUEST, "DIGEST_INVALID", "only sha256 digests are supported")
            })?;
            let asset = blob(state, &repo, &image, hex, 0)
                .await
                .map_err(|e| RegistryError::from_error(e, "BLOB_UNKNOWN"))?
                .ok_or_else(|| {
                    RegistryError::new(StatusCode::NOT_FOUND, "BLOB_UNKNOWN", format!("blob unknown: {digest}"))
                })?;
            let mut resp = content::serve_asset(state, &asset, method, headers)
                .await
                .map_err(|e| RegistryError::from_error(e, "BLOB_UNKNOWN"))?;
            resp.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
            if let Ok(v) = HeaderValue::from_str(&format!("sha256:{hex}")) {
                resp.headers_mut().insert("docker-content-digest", v);
            }
            Ok(resp)
        }
        Target::Tags(name) => {
            let (repo, image) = route(state, name).await?;
            if !principal.can_read_repo(state, &repo) {
                return Err(RegistryError::new(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", "authentication required"));
            }
            let tags = tags(state, &repo, &image, 0).await.map_err(|e| RegistryError::from_error(e, "NAME_UNKNOWN"))?;
            match tags {
                Some(list) => Ok(Json(json!({ "name": name, "tags": list })).into_response()),
                None => Err(RegistryError::new(
                    StatusCode::NOT_FOUND,
                    "NAME_UNKNOWN",
                    format!("repository name not known: {name}"),
                )),
            }
        }
        Target::Upload(_) => unreachable!("handled above"),
    }
}

/// Resolve `{repo}/{image}` (path-based) or `{image}` via the default repository.
async fn route(state: &AppState, name: &str) -> std::result::Result<(Repository, String), RegistryError> {
    let not_found =
        || RegistryError::new(StatusCode::NOT_FOUND, "NAME_UNKNOWN", format!("repository name not known: {name}"));
    let internal = |e: Error| RegistryError::from_error(e, "NAME_UNKNOWN");
    if let Some((first, rest)) = name.split_once('/')
        && let Some(repo) = state.db.get_repo_by_name(first).await.map_err(internal)?
        && repo.format == Format::Docker
    {
        if !valid_image_name(rest) {
            return Err(RegistryError::new(
                StatusCode::BAD_REQUEST,
                "NAME_INVALID",
                format!("invalid image name: {rest}"),
            ));
        }
        return Ok((repo, rest.to_string()));
    }
    let default = state.db.get_setting(SETTING_DOCKER_DEFAULT_REPO).await.map_err(internal)?.filter(|s| !s.is_empty());
    let Some(default) = default else { return Err(not_found()) };
    let repo = state
        .db
        .get_repo_by_name(&default)
        .await
        .map_err(internal)?
        .filter(|r| r.format == Format::Docker)
        .ok_or_else(not_found)?;
    if !valid_image_name(name) {
        return Err(RegistryError::new(StatusCode::BAD_REQUEST, "NAME_INVALID", format!("invalid image name: {name}")));
    }
    Ok((repo, name.to_string()))
}

fn manifest_media_type(asset: &Asset, body: Option<&[u8]>) -> String {
    if asset.content_type.starts_with("application/vnd.") {
        return asset.content_type.clone();
    }
    body.and_then(|b| serde_json::from_slice::<Value>(b).ok())
        .and_then(|v| v.get("mediaType").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| "application/vnd.oci.image.manifest.v1+json".to_string())
}

async fn serve_manifest(
    state: &AppState,
    asset: &Asset,
    method: &Method,
    headers: &HeaderMap,
) -> std::result::Result<Response, RegistryError> {
    let err = |e: Error| RegistryError::from_error(e, "MANIFEST_UNKNOWN");
    let media_type = if asset.content_type.starts_with("application/vnd.") {
        asset.content_type.clone()
    } else {
        let body = content::read_asset(state, asset).await.map_err(err)?;
        manifest_media_type(asset, Some(&body))
    };
    let mut resp = content::serve_asset(state, asset, method, headers).await.map_err(err)?;
    let h = resp.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&media_type) {
        h.insert(header::CONTENT_TYPE, v);
    }
    if let Ok(v) = HeaderValue::from_str(&format!("sha256:{}", asset.blob.sha256)) {
        h.insert("docker-content-digest", v);
    }
    Ok(resp)
}

/// Make sure a manifest fetched by tag is also addressable by its digest.
async fn link_digest(state: &AppState, repo: &Repository, image: &str, tagged: &Asset) -> Result<()> {
    let path = format!("{image}/manifests/sha256:{}", tagged.blob.sha256);
    if state.db.get_asset(&repo.id, &path).await?.is_none() {
        state
            .db
            .put_asset(NewAsset {
                repo_id: &repo.id,
                path: &path,
                blob: &tagged.blob,
                content_type: &tagged.content_type,
                uploader: "proxy",
                attributes: json!({ "cached_at": now_millis() }),
            })
            .await?;
    }
    Ok(())
}

fn manifest<'a>(
    state: &'a AppState,
    repo: &'a Repository,
    image: &'a str,
    reference: &'a str,
    depth: usize,
) -> futures::future::BoxFuture<'a, Result<Option<Asset>>> {
    Box::pin(async move {
        if !repo.online {
            return Ok(None);
        }
        let by_digest = sha256_of(reference);
        if by_digest.is_none() && !valid_tag(reference) {
            return Err(Error::bad_request(format!("invalid reference: {reference}")));
        }
        match repo.kind {
            RepoKind::Proxy => {
                let base = remote_base(repo);
                let url = format!("{base}/v2/{}/manifests/{reference}", upstream_name(&base, image));
                let (cache_path, opts) = match by_digest {
                    Some(hex) => (
                        format!("{image}/manifests/{reference}"),
                        FetchOpts {
                            metadata: false,
                            accept: Some(MANIFEST_ACCEPT),
                            registry_auth: true,
                            keep_content_type: true,
                            expect_sha256: Some(hex),
                            stale_before: None,
                        },
                    ),
                    None => (
                        format!("{image}/tags/{reference}"),
                        FetchOpts {
                            metadata: true,
                            accept: Some(MANIFEST_ACCEPT),
                            registry_auth: true,
                            keep_content_type: true,
                            ..Default::default()
                        },
                    ),
                };
                let asset = proxy::fetch_opts(state, repo, &cache_path, &url, opts).await?;
                if let (Some(a), None) = (&asset, by_digest) {
                    link_digest(state, repo, image, a).await?;
                }
                Ok(asset)
            }
            RepoKind::Group => {
                let mut failure = None;
                for m in content::group_members(state, repo, depth).await? {
                    match manifest(state, &m, image, reference, depth + 1).await {
                        Ok(Some(a)) => return Ok(Some(a)),
                        Ok(None) => {}
                        Err(e) => failure = Some(content::member_failed(&m, e)),
                    }
                }
                failure.map_or(Ok(None), Err)
            }
            RepoKind::Hosted => Ok(None),
        }
    })
}

fn blob<'a>(
    state: &'a AppState,
    repo: &'a Repository,
    image: &'a str,
    hex: &'a str,
    depth: usize,
) -> futures::future::BoxFuture<'a, Result<Option<Asset>>> {
    Box::pin(async move {
        if !repo.online {
            return Ok(None);
        }
        let path = format!("{image}/blobs/sha256:{hex}");
        match repo.kind {
            RepoKind::Proxy => {
                if let Some(a) = state.db.get_asset(&repo.id, &path).await? {
                    return Ok(Some(a));
                }
                // Layers are content-addressed: reuse an identical blob already
                // stored for another image or repository instead of downloading.
                if let Some(info) = state.db.get_blob(hex).await?
                    && state.blobs.exists(hex).await?
                {
                    let _guard = state.gc_lock.read().await;
                    let (asset, _) = state
                        .db
                        .put_asset(NewAsset {
                            repo_id: &repo.id,
                            path: &path,
                            blob: &info,
                            content_type: "application/octet-stream",
                            uploader: "proxy",
                            attributes: json!({ "cached_at": now_millis(), "deduplicated": true }),
                        })
                        .await?;
                    return Ok(Some(asset));
                }
                let base = remote_base(repo);
                let url = format!("{base}/v2/{}/blobs/sha256:{hex}", upstream_name(&base, image));
                let opts =
                    FetchOpts { metadata: false, registry_auth: true, expect_sha256: Some(hex), ..Default::default() };
                proxy::fetch_opts(state, repo, &path, &url, opts).await
            }
            RepoKind::Group => {
                let mut failure = None;
                for m in content::group_members(state, repo, depth).await? {
                    match blob(state, &m, image, hex, depth + 1).await {
                        Ok(Some(a)) => return Ok(Some(a)),
                        Ok(None) => {}
                        Err(e) => failure = Some(content::member_failed(&m, e)),
                    }
                }
                failure.map_or(Ok(None), Err)
            }
            RepoKind::Hosted => Ok(None),
        }
    })
}

fn tags<'a>(
    state: &'a AppState,
    repo: &'a Repository,
    image: &'a str,
    depth: usize,
) -> futures::future::BoxFuture<'a, Result<Option<Vec<String>>>> {
    Box::pin(async move {
        if !repo.online {
            return Ok(None);
        }
        match repo.kind {
            RepoKind::Proxy => {
                let base = remote_base(repo);
                let url = format!("{base}/v2/{}/tags/list", upstream_name(&base, image));
                let opts = FetchOpts {
                    metadata: true,
                    registry_auth: true,
                    accept: Some("application/json"),
                    ..Default::default()
                };
                let Some(asset) =
                    proxy::fetch_opts(state, repo, &format!("{image}/_tags/list.json"), &url, opts).await?
                else {
                    return Ok(None);
                };
                let doc: Value = serde_json::from_slice(&content::read_asset(state, &asset).await?)?;
                Ok(Some(
                    doc.get("tags")
                        .and_then(Value::as_array)
                        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                        .unwrap_or_default(),
                ))
            }
            RepoKind::Group => {
                let mut all = BTreeSet::new();
                let mut found = false;
                for m in content::group_members(state, repo, depth).await? {
                    match tags(state, &m, image, depth + 1).await {
                        Ok(Some(list)) => {
                            found = true;
                            all.extend(list);
                        }
                        Ok(None) => {}
                        Err(e) => {
                            content::member_failed(&m, e);
                        }
                    }
                }
                Ok(found.then(|| all.into_iter().collect()))
            }
            RepoKind::Hosted => Ok(None),
        }
    })
}

/// Images that have been pulled through each Docker repository.
async fn catalog(state: &AppState, principal: &crate::auth::Principal) -> Result<Response> {
    let mut names = BTreeSet::new();
    for repo in state
        .db
        .list_repos()
        .await?
        .into_iter()
        .filter(|r| r.format == Format::Docker && r.kind == RepoKind::Proxy && principal.can_read_repo(state, r))
    {
        for a in state.db.list_assets_under(&repo.id, "").await? {
            for marker in ["/manifests/", "/tags/"] {
                if let Some((image, _)) = a.path.split_once(marker) {
                    names.insert(format!("{}/{image}", repo.name));
                }
            }
        }
    }
    Ok(Json(json!({ "repositories": names })).into_response())
}

/// Used by the generic `/repository/{name}/…` dispatcher: Docker clients
/// must use `/v2/`, so point them there.
pub async fn handle_repository_path(ctx: super::Ctx) -> Result<Response> {
    let host = ctx.base_url();
    let host = host.trim_start_matches("http://").trim_start_matches("https://");
    let msg = format!("Docker clients use the registry API: docker pull {host}/{}/<image>:<tag>\n", ctx.repo.name);
    Ok((StatusCode::NOT_FOUND, Body::from(msg)).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets() {
        assert_eq!(parse_target(""), Some(Target::Base));
        assert_eq!(parse_target("/"), Some(Target::Base));
        assert_eq!(parse_target("/_catalog"), Some(Target::Catalog));
        assert_eq!(
            parse_target("/hub/library/nginx/manifests/1.27"),
            Some(Target::Manifest("hub/library/nginx", "1.27"))
        );
        assert_eq!(parse_target("/hub/a/blobs/sha256:00"), Some(Target::Blob("hub/a", "sha256:00")));
        assert_eq!(parse_target("/hub/a/tags/list"), Some(Target::Tags("hub/a")));
        assert_eq!(parse_target("/hub/a/blobs/uploads/"), Some(Target::Upload("hub/a")));
        // an image literally named "manifests" inside a path still parses on the last marker
        assert_eq!(
            parse_target("/hub/manifests/x/manifests/latest"),
            Some(Target::Manifest("hub/manifests/x", "latest"))
        );
    }

    #[test]
    fn names_and_digests() {
        assert!(valid_image_name("library/nginx"));
        assert!(valid_image_name("my-org/sub.team/app_1"));
        assert!(!valid_image_name("Library/nginx"));
        assert!(!valid_image_name("a//b"));
        assert!(valid_tag("1.27-alpine"));
        assert!(!valid_tag(".hidden"));
        assert!(sha256_of(&format!("sha256:{}", "a".repeat(64))).is_some());
        assert!(sha256_of("sha256:abc").is_none());
        assert_eq!(upstream_name("https://registry-1.docker.io", "nginx"), "library/nginx");
        assert_eq!(upstream_name("https://registry-1.docker.io", "bitnami/redis"), "bitnami/redis");
        assert_eq!(upstream_name("https://ghcr.io", "nginx"), "nginx");
    }
}
