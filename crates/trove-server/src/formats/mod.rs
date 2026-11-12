//! Package-manager protocol endpoints, mounted at `/repository/{name}/...`.

pub mod apt;
pub mod cargo;
pub mod docker;
pub mod git;
pub mod maven;
pub mod npm;
pub mod path_proxy;
pub mod pypi;
pub mod raw;
pub mod skills;
pub mod yum;
pub mod yum_group;

use std::collections::HashMap;

use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, Method, Uri};
use axum::response::Response;
use bytes::Bytes;
use futures::TryStreamExt;
use trove_core::model::{Format, RepoKind, Repository};
use trove_core::{Error, Result};

use crate::auth::{self, Principal};
use crate::error::format_error;
use crate::state::AppState;

/// Everything a format handler needs about the current request.
pub struct Ctx {
    pub state: AppState,
    pub repo: Repository,
    /// Percent-decoded path below the repository root, without leading slash.
    pub path: String,
    pub method: Method,
    pub headers: HeaderMap,
    pub uri: Uri,
    pub principal: Principal,
    /// Wrapped so `&Ctx` is `Send` across awaits (`Body` is not `Sync`).
    body: std::sync::Mutex<Option<Body>>,
    /// Original request extensions (carries the body-limit configuration).
    pub extensions: axum::http::Extensions,
}

impl Ctx {
    pub fn query_param(&self, key: &str) -> Option<String> {
        self.uri.query().and_then(|q| {
            url::form_urlencoded::parse(q.as_bytes()).find(|(k, _)| k == key).map(|(_, v)| v.into_owned())
        })
    }

    pub fn base_url(&self) -> String {
        crate::content::base_url(&self.state, &self.headers)
    }

    pub fn repo_url(&self) -> String {
        crate::content::repo_url(&self.state, &self.headers, &self.repo)
    }

    /// Take the request body as a byte stream.
    pub fn body_stream(
        &mut self,
    ) -> impl futures::Stream<Item = std::result::Result<Bytes, axum::Error>> + Send + use<> {
        self.take_body().into_data_stream()
    }

    /// Buffer the request body, bounded by `limit` bytes.
    pub async fn body_bytes(&mut self, limit: usize) -> Result<Bytes> {
        let body = self.take_body();
        axum::body::to_bytes(body, limit).await.map_err(|e| {
            if e.to_string().contains("length limit") {
                Error::PayloadTooLarge
            } else {
                Error::bad_request(format!("reading body: {e}"))
            }
        })
    }

    pub fn take_body(&mut self) -> Body {
        self.body.get_mut().ok().and_then(Option::take).unwrap_or_else(Body::empty)
    }

    pub fn is_read(&self) -> bool {
        matches!(self.method, Method::GET | Method::HEAD)
    }
}

pub async fn dispatch_root(state: State<AppState>, Path(repo): Path<String>, req: Request) -> Response {
    dispatch_inner(state.0, repo, String::new(), req).await
}

pub async fn dispatch(state: State<AppState>, Path(params): Path<HashMap<String, String>>, req: Request) -> Response {
    let repo = params.get("repo").cloned().unwrap_or_default();
    let path = params.get("path").cloned().unwrap_or_default();
    dispatch_inner(state.0, repo, path, req).await
}

async fn dispatch_inner(state: AppState, repo_name: String, path: String, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let repo = match state.repo(&repo_name).await {
        Ok(r) => r,
        Err(e) => return format_error(e, false),
    };
    let json_errors = matches!(repo.format, Format::Npm);
    let principal = match auth::authenticate(&state, &parts.headers).await {
        Ok(p) => p,
        Err(e) => return render_error(repo.format, e, json_errors),
    };
    // Browser sessions may upload and delete, but only with the UI's CSRF header.
    let safe = matches!(parts.method, Method::GET | Method::HEAD | Method::OPTIONS);
    if principal.method == auth::AuthMethod::Session && !safe && !parts.headers.contains_key(auth::CSRF_HEADER) {
        return render_error(repo.format, Error::forbidden("missing X-Trove-Request header"), json_errors);
    }
    // Preserve a trailing slash: it is meaningful for directory listings and PyPI.
    let raw_path = parts.uri.path();
    let mut path = path.trim_start_matches('/').to_string();
    if raw_path.ends_with('/') && !path.is_empty() && !path.ends_with('/') {
        path.push('/');
    }
    // Group deployment: uploads sent to a group go to its writable member.
    let group_format = repo.format;
    let repo = match writable_target(&state, repo, &parts.method, &path).await {
        Ok(r) => r,
        Err(e) => return render_error(group_format, e, json_errors),
    };
    let format = repo.format;
    let ctx = Ctx {
        state,
        repo,
        path,
        method: parts.method,
        headers: parts.headers,
        uri: parts.uri,
        principal,
        body: std::sync::Mutex::new(Some(body)),
        extensions: parts.extensions,
    };
    let result = match format {
        Format::Raw => raw::handle(ctx).await,
        Format::Maven => maven::handle(ctx).await,
        Format::Npm => npm::handle(ctx).await,
        Format::Pypi => pypi::handle(ctx).await,
        Format::Cargo => cargo::handle(ctx).await,
        Format::Apt => apt::handle(ctx).await,
        Format::Yum => yum::handle(ctx).await,
        Format::Git => git::handle(ctx).await,
        Format::Skills => skills::handle(ctx).await,
        Format::Docker => docker::handle_repository_path(ctx).await,
    };
    result.unwrap_or_else(|e| render_error(format, e, json_errors))
}

/// For a write to a group with a writable member, the member to write to.
async fn writable_target(state: &AppState, repo: Repository, method: &Method, path: &str) -> Result<Repository> {
    let write = !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS);
    // npm audits are POSTed but are reads, answered through a proxy member.
    let npm_read = repo.format == Format::Npm && path.starts_with("-/npm/v1/security/");
    let Some(member) = repo.writable_member().filter(|_| write && !npm_read && repo.kind == RepoKind::Group) else {
        return Ok(repo);
    };
    let target = state.repo(member).await.map_err(|_| {
        Error::internal(format!("writable member '{member}' of group '{}' no longer exists", repo.name))
    })?;
    if target.kind != RepoKind::Hosted || target.format != repo.format {
        return Err(Error::internal(format!(
            "writable member '{member}' of group '{}' is not a hosted {} repository",
            repo.name, repo.format
        )));
    }
    Ok(target)
}

fn render_error(format: Format, e: Error, json: bool) -> Response {
    if format == Format::Cargo {
        return cargo::error_response(e);
    }
    format_error(e, json)
}

/// Shared helper: a request stream of body chunks mapped to io errors.
pub fn io_stream(
    s: impl futures::Stream<Item = std::result::Result<Bytes, axum::Error>> + Send,
) -> impl futures::Stream<Item = std::io::Result<Bytes>> + Send {
    s.map_err(std::io::Error::other)
}
