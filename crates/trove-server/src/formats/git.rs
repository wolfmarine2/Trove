//! Git proxy: a pull-through cache for Git smart-HTTP remotes (GitHub,
//! GitLab, Gitee, Gitea, …), so intranet clients can clone repositories,
//! install plugins and read marketplace files without reaching the remote.
//!
//! ```text
//! git config --global url."http://trove.example.com/repository/github/".insteadOf "https://github.com/"
//! git clone https://github.com/owner/repo.git
//! ```
//!
//! Each upstream repository is kept as a bare mirror (branches and tags) on
//! local disk and refreshed with `git fetch` once it is older than the
//! repository's metadata max age. Clients are served by `git upload-pack`
//! from the mirror. When the remote is down, the last mirrored state is
//! served. Pushes are refused.
//!
//! Besides the Git protocol the mirror also answers, without the remote:
//!
//! * `<owner>/<repo>/archive/<ref>.tar.gz|.zip` and the codeload form
//!   `<owner>/<repo>/tar.gz/<ref>` — source archives (npm/pnpm tarballs);
//! * `<owner>/<repo>/raw/<ref>/<path>` — a single file, like
//!   `raw.githubusercontent.com`.
//!
//! For remotes with nested groups (GitLab), separate the repository path with
//! `/-/`, e.g. `group/sub/project/-/raw/main/README.md`.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use axum::body::Body;
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use bytes::Bytes;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use trove_core::model::{DEFAULT_METADATA_MAX_AGE_SECS, OutboundMode, Repository};
use trove_core::util::now_millis;
use trove_core::{Error, Result};

use super::Ctx;
use crate::audit;
use crate::state::{AppState, Metrics};

/// Largest `git-upload-pack` request accepted (want/have negotiation).
const MAX_REQUEST: usize = 64 * 1024 * 1024;
/// Marker file whose mtime records the last successful fetch.
const MARKER: &str = "trove-fetched";
/// A forced refresh (unknown commit requested) is skipped if the mirror was
/// fetched this recently.
const FORCE_GRACE: Duration = Duration::from_secs(10);

#[derive(Debug, PartialEq)]
enum Route {
    InfoRefs,
    UploadPack,
    ReceivePack,
    Archive {
        reference: String,
        format: ArchiveFormat,
    },
    Raw {
        rest: Vec<String>,
    },
    /// The repository itself (`DELETE` evicts the mirror).
    Repo,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum ArchiveFormat {
    TarGz,
    Zip,
    Tar,
}

impl ArchiveFormat {
    fn git_name(self) -> &'static str {
        match self {
            ArchiveFormat::TarGz => "tar.gz",
            ArchiveFormat::Zip => "zip",
            ArchiveFormat::Tar => "tar",
        }
    }
    fn content_type(self) -> &'static str {
        match self {
            ArchiveFormat::TarGz => "application/gzip",
            ArchiveFormat::Zip => "application/zip",
            ArchiveFormat::Tar => "application/x-tar",
        }
    }
    fn strip(name: &str) -> Option<(&str, ArchiveFormat)> {
        [
            (".tar.gz", ArchiveFormat::TarGz),
            (".tgz", ArchiveFormat::TarGz),
            (".zip", ArchiveFormat::Zip),
            (".tar", ArchiveFormat::Tar),
        ]
        .into_iter()
        .find_map(|(ext, f)| name.strip_suffix(ext).filter(|s| !s.is_empty()).map(|s| (s, f)))
    }
}

/// Split a request path into the repository path (without `.git`) and what is asked of it.
fn parse(path: &str) -> Result<(String, Route)> {
    let path = path.trim_matches('/');
    let (repo, route) = if let Some(p) = path.strip_suffix("/info/refs") {
        (p.to_string(), Route::InfoRefs)
    } else if let Some(p) = path.strip_suffix("/git-upload-pack") {
        (p.to_string(), Route::UploadPack)
    } else if let Some(p) = path.strip_suffix("/git-receive-pack") {
        (p.to_string(), Route::ReceivePack)
    } else if let Some((repo, tail)) = path.split_once("/-/") {
        let segs: Vec<&str> = tail.split('/').collect();
        (repo.to_string(), tail_route(&segs, true)?)
    } else {
        let segs: Vec<&str> = path.split('/').collect();
        if segs.len() <= 2 {
            (path.to_string(), Route::Repo)
        } else {
            (segs[..2].join("/"), tail_route(&segs[2..], false)?)
        }
    };
    Ok((repo_path(&repo)?, route))
}

fn tail_route(segs: &[&str], gitlab: bool) -> Result<Route> {
    let unknown = || Error::not_found(format!("unknown git proxy path '{}'", segs.join("/")));
    match segs {
        [] | [""] => Ok(Route::Repo),
        ["raw", rest @ ..] if rest.len() >= 2 => Ok(Route::Raw { rest: rest.iter().map(|s| s.to_string()).collect() }),
        // GitHub codeload: tar.gz/<ref>, zip/<ref>
        [kind @ ("tar.gz" | "zip" | "legacy.tar.gz" | "legacy.zip"), rest @ ..] if !rest.is_empty() => {
            let format = if kind.ends_with("zip") { ArchiveFormat::Zip } else { ArchiveFormat::TarGz };
            Ok(Route::Archive { reference: rest.join("/"), format })
        }
        ["archive", rest @ ..] if !rest.is_empty() => {
            let last = rest[rest.len() - 1];
            let (stem, format) = ArchiveFormat::strip(last).ok_or_else(unknown)?;
            let reference = if gitlab && rest.len() >= 2 {
                // GitLab: archive/<ref>/<project>-<ref>.tar.gz
                rest[..rest.len() - 1].join("/")
            } else {
                // GitHub: archive/<ref>.tar.gz, archive/refs/tags/v1.0.tar.gz
                let mut r: Vec<&str> = rest[..rest.len() - 1].to_vec();
                r.push(stem);
                r.join("/")
            };
            Ok(Route::Archive { reference, format })
        }
        _ => Err(unknown()),
    }
}

/// Validate and normalise an upstream repository path (`owner/name`, no `.git`).
fn repo_path(raw: &str) -> Result<String> {
    let trimmed = raw.trim_matches('/');
    let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    if trimmed.is_empty() {
        return Err(Error::not_found("a repository path such as owner/name is required"));
    }
    for seg in trimmed.split('/') {
        let ok = !seg.is_empty()
            && !seg.starts_with('.')
            && !seg.ends_with(".lock")
            && seg.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        if !ok {
            return Err(Error::bad_request(format!("invalid repository path segment '{seg}'")));
        }
    }
    Ok(trimmed.to_string())
}

/// Refs and revisions supplied by clients must not look like options or ranges.
fn valid_ref(r: &str) -> bool {
    !r.is_empty()
        && r.len() <= 255
        && !r.starts_with('-')
        && !r.contains("..")
        && !r.contains("@{")
        && r.chars().all(|c| !c.is_control() && !matches!(c, ' ' | ':' | '~' | '^' | '?' | '*' | '[' | '\\'))
}

struct Mirror {
    /// Repository path below the remote, e.g. `owner/name`.
    path: String,
    dir: PathBuf,
    upstream: String,
}

impl Mirror {
    fn new(state: &AppState, repo: &Repository, path: String) -> Self {
        let dir = state.cfg.git_mirror_dir().join(&repo.id).join(format!("{path}.git"));
        let remote = repo.proxy().map(|p| p.remote_url.as_str()).unwrap_or_default();
        let upstream = format!("{}.git", crate::content::join_url(remote, &path));
        Self { path, dir, upstream }
    }

    fn exists(&self) -> bool {
        self.dir.join("HEAD").is_file() && self.dir.join(MARKER).is_file()
    }

    fn fetched_at(&self) -> Option<SystemTime> {
        std::fs::metadata(self.dir.join(MARKER)).and_then(|m| m.modified()).ok()
    }

    fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }
}

/// Root CA bundle for git: the system bundle plus `http_client.ca_bundle`
/// (git's `http.sslCAInfo` replaces the defaults instead of adding to them).
fn ca_bundle(state: &AppState) -> Option<PathBuf> {
    static BUNDLE: OnceLock<Option<PathBuf>> = OnceLock::new();
    BUNDLE
        .get_or_init(|| {
            let extra = &state.cfg.http_client.ca_bundle;
            if extra.is_empty() {
                return None;
            }
            let mut pem = Vec::new();
            for sys in ["/etc/ssl/certs/ca-certificates.crt", "/etc/pki/tls/certs/ca-bundle.crt", "/etc/ssl/cert.pem"] {
                if let Ok(b) = std::fs::read(sys) {
                    pem.extend(b);
                    pem.push(b'\n');
                    break;
                }
            }
            match std::fs::read(extra) {
                Ok(b) => pem.extend(b),
                Err(e) => {
                    tracing::warn!(path = %extra, error = %e, "cannot read http_client.ca_bundle for git");
                    return None;
                }
            }
            let root = state.cfg.git_mirror_dir();
            let path = root.join("ca-bundle.pem");
            std::fs::create_dir_all(&root).and_then(|_| std::fs::write(&path, pem)).ok().map(|_| path)
        })
        .clone()
}

fn proxy_url(url: &str, username: &str, password: &str) -> String {
    match url::Url::parse(url.trim()) {
        Ok(mut u) if !username.is_empty() => {
            let _ = u.set_username(username);
            let _ = u.set_password(Some(password));
            u.to_string()
        }
        _ => url.trim().to_string(),
    }
}

/// A `git` command isolated from user/system configuration. Settings travel
/// in `GIT_CONFIG_*` environment variables so secrets never show up in `ps`.
fn git(state: &AppState, repo: &Repository) -> Command {
    let mut cmd = Command::new(&state.cfg.git.binary);
    cmd.env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_ASKPASS", "true")
        .env("GIT_ALLOW_PROTOCOL", "http:https")
        .env_remove("GIT_DIR")
        .env_remove("GIT_PROTOCOL")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let hc = &state.cfg.http_client;
    let mut config: Vec<(&str, String)> = vec![
        ("http.lowSpeedLimit", "1".into()),
        ("http.lowSpeedTime", hc.timeout_secs.max(1).to_string()),
        ("core.askPass", "true".into()),
        ("uploadpack.allowFilter", "true".into()),
        ("uploadpack.allowReachableSHA1InWant", "true".into()),
    ];
    let proxy_env = ["http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY", "all_proxy", "ALL_PROXY"];
    let mut set_proxy = |cmd: &mut Command, url: String, no_proxy: &str| {
        for k in proxy_env {
            cmd.env_remove(k);
        }
        config.push(("http.proxy", url));
        if !no_proxy.trim().is_empty() {
            cmd.env("NO_PROXY", no_proxy).env("no_proxy", no_proxy);
        }
    };
    if let Some(p) = repo.proxy() {
        let o = &p.outbound;
        match o.mode {
            OutboundMode::Inherit if !hc.proxy.is_empty() => {
                set_proxy(&mut cmd, proxy_url(&hc.proxy, "", ""), &hc.no_proxy)
            }
            OutboundMode::Inherit => {} // environment proxies pass through, as for HTTP proxies
            OutboundMode::Direct => {
                for k in proxy_env.iter().chain(&["no_proxy", "NO_PROXY"]) {
                    cmd.env_remove(k);
                }
            }
            OutboundMode::Custom => set_proxy(&mut cmd, proxy_url(&o.url, &o.username, &o.password), &o.no_proxy),
        }
        if !p.username.is_empty() {
            let token = base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", p.username, p.password));
            config.push(("http.extraHeader", format!("Authorization: Basic {token}")));
        }
    }
    if let Some(ca) = ca_bundle(state) {
        config.push(("http.sslCAInfo", ca.display().to_string()));
    }
    cmd.env("GIT_CONFIG_COUNT", config.len().to_string());
    for (i, (k, v)) in config.into_iter().enumerate() {
        cmd.env(format!("GIT_CONFIG_KEY_{i}"), k).env(format!("GIT_CONFIG_VALUE_{i}"), v);
    }
    cmd
}

/// Run a git command to completion, returning stdout or the (trimmed) stderr.
async fn run(mut cmd: Command, timeout: Duration) -> std::result::Result<Vec<u8>, String> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = cmd.spawn().map_err(|e| format!("cannot run git: {e}"))?;
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Err(_) => Err(format!("timed out after {}s", timeout.as_secs())),
        Ok(Err(e)) => Err(e.to_string()),
        Ok(Ok(out)) if out.status.success() => Ok(out.stdout),
        Ok(Ok(out)) => {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            Err(if err.is_empty() { out.status.to_string() } else { err })
        }
    }
}

fn looks_missing(stderr: &str) -> bool {
    let e = stderr.to_ascii_lowercase();
    [
        "not found",
        "could not read username",
        "authentication failed",
        "does not appear to be a git repository",
        "error: 404",
        "error: 401",
    ]
    .iter()
    .any(|s| e.contains(s))
}

fn is_fresh(repo: &Repository, m: &Mirror) -> bool {
    let Some(at) = m.fetched_at() else { return false };
    let p = repo.proxy();
    let max_age = p.map(|p| p.metadata_max_age_secs).filter(|v| *v > 0).unwrap_or(DEFAULT_METADATA_MAX_AGE_SECS);
    let at_ms = at.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
    at_ms > p.map(|p| p.invalidated_at).unwrap_or(0) && now_millis() - at_ms < (max_age as i64) * 1000
}

/// Make sure the mirror exists and is fresh enough. `force` asks for a fetch
/// even within the max age (a client wants a commit the mirror lacks).
async fn ensure(state: &AppState, repo: &Repository, m: &Mirror, force: bool) -> Result<()> {
    let recently = |m: &Mirror| m.fetched_at().and_then(|t| t.elapsed().ok()).is_some_and(|d| d < FORCE_GRACE);
    let satisfied = |m: &Mirror| m.exists() && if force { recently(m) } else { is_fresh(repo, m) };
    if satisfied(m) {
        Metrics::inc(&state.metrics.proxy_hits);
        return Ok(());
    }
    let offline = repo.proxy().is_none_or(|p| p.offline) || !repo.online;
    if offline {
        return if m.exists() { Ok(()) } else { Err(Error::not_found(format!("{} is not mirrored", m.path))) };
    }
    let key = format!("git:{}:{}", repo.id, m.path);
    if !m.exists() && state.is_negative(&key).await {
        return Err(Error::not_found(format!("{} not found upstream", m.path)));
    }
    // A remote that just failed is not retried on every request.
    let failed_key = format!("{key}:failed");
    if m.exists() && !force && state.is_negative(&failed_key).await {
        return Ok(());
    }
    let lock = state.flight_lock(&key).await;
    let _held = lock.lock().await;
    if satisfied(m) {
        Metrics::inc(&state.metrics.proxy_hits);
        return Ok(());
    }
    Metrics::inc(&state.metrics.proxy_fetches);
    let existed = m.exists();
    tracing::debug!(repo = %repo.name, upstream = %m.upstream, existed, "fetching git mirror");
    match fetch(state, repo, m).await {
        Ok(()) => Ok(()),
        Err(detail) => {
            Metrics::inc(&state.metrics.upstream_errors);
            tracing::warn!(repo = %repo.name, upstream = %m.upstream, error = %detail, "git fetch failed");
            if existed {
                state.set_negative(&failed_key).await;
                Ok(()) // serve the last mirrored state
            } else if looks_missing(&detail) {
                state.set_negative(&key).await;
                Err(Error::not_found(format!("{} not found upstream", m.path)))
            } else {
                Err(Error::Upstream(format!("{}: {detail}", m.upstream)))
            }
        }
    }
}

async fn fetch(state: &AppState, repo: &Repository, m: &Mirror) -> std::result::Result<(), String> {
    let timeout = Duration::from_secs(state.cfg.git.fetch_timeout_secs.max(10));
    let local = Duration::from_secs(60);
    let target = if m.exists() {
        m.dir.clone()
    } else {
        // Build a new mirror next to its final place, then move it in.
        let parent = m.dir.parent().ok_or("invalid mirror path")?;
        std::fs::create_dir_all(parent).map_err(|e| format!("creating {}: {e}", parent.display()))?;
        let tmp = parent.join(format!(".{}.tmp-{}", m.name(), now_millis()));
        let _ = std::fs::remove_dir_all(&m.dir);
        let mut init = git(state, repo);
        init.args(["init", "--bare", "--quiet"]).arg(&tmp);
        run(init, local).await?;
        for (key, value, add) in [
            ("remote.origin.url", m.upstream.as_str(), false),
            ("remote.origin.fetch", "+refs/heads/*:refs/heads/*", false),
            ("remote.origin.fetch", "+refs/tags/*:refs/tags/*", true),
        ] {
            let mut c = git(state, repo);
            c.arg("--git-dir").arg(&tmp).arg("config");
            if add {
                c.arg("--add");
            }
            c.args([key, value]);
            run(c, local).await?;
        }
        tmp
    };
    let mut f = git(state, repo);
    f.arg("--git-dir").arg(&target).args(["fetch", "--prune", "--quiet", "origin"]);
    let result = run(f, timeout).await;
    if result.is_ok() {
        set_head(state, repo, &target).await;
    }
    if target != m.dir {
        if let Err(e) = result {
            let _ = std::fs::remove_dir_all(&target);
            return Err(e);
        }
        std::fs::rename(&target, &m.dir).map_err(|e| format!("installing mirror: {e}"))?;
    }
    result?;
    std::fs::write(m.dir.join(MARKER), now_millis().to_string()).map_err(|e| format!("writing marker: {e}"))?;
    Ok(())
}

/// Point HEAD at the upstream default branch so plain clones check it out.
async fn set_head(state: &AppState, repo: &Repository, dir: &Path) {
    let mut c = git(state, repo);
    c.arg("--git-dir").arg(dir).args(["ls-remote", "--symref", "origin", "HEAD"]);
    let Ok(out) = run(c, Duration::from_secs(120)).await else { return };
    let text = String::from_utf8_lossy(&out);
    let head = text.lines().find_map(|l| l.strip_prefix("ref: ")?.strip_suffix("\tHEAD").map(str::to_string));
    if let Some(head) = head.filter(|h| h.starts_with("refs/heads/") && valid_ref(h)) {
        let mut c = git(state, repo);
        c.arg("--git-dir").arg(dir).args(["symbolic-ref", "HEAD", &head]);
        let _ = run(c, Duration::from_secs(60)).await;
    }
}

async fn resolve_commit(state: &AppState, repo: &Repository, m: &Mirror, reference: &str) -> Option<String> {
    if !valid_ref(reference) {
        return None;
    }
    let mut c = git(state, repo);
    c.arg("--git-dir")
        .arg(&m.dir)
        .args(["rev-parse", "--verify", "--quiet", "--end-of-options"])
        .arg(format!("{reference}^{{commit}}"));
    let out = run(c, Duration::from_secs(60)).await.ok()?;
    Some(String::from_utf8_lossy(&out).trim().to_string()).filter(|s| !s.is_empty())
}

pub async fn handle(mut ctx: Ctx) -> Result<Response> {
    let (path, route) = parse(&ctx.path)?;
    let m = Mirror::new(&ctx.state, &ctx.repo, path);
    match (&ctx.method, route) {
        (_, Route::ReceivePack) => Err(Error::forbidden("git proxy repositories are read-only")),
        (&Method::DELETE, Route::Repo) => evict(&ctx, &m).await,
        (&Method::GET | &Method::HEAD, Route::InfoRefs) => {
            ctx.principal.require_repo_read(&ctx.state, &ctx.repo)?;
            match ctx.query_param("service").as_deref() {
                Some("git-upload-pack") => {}
                Some("git-receive-pack") => return Err(Error::forbidden("git proxy repositories are read-only")),
                _ => return Err(Error::forbidden("only the smart HTTP protocol is supported; upgrade git")),
            }
            ensure(&ctx.state, &ctx.repo, &m, false).await?;
            let mut c = git(&ctx.state, &ctx.repo);
            c.args(["upload-pack", "--stateless-rpc", "--advertise-refs"]).arg(&m.dir);
            let refs = run(c, Duration::from_secs(120)).await.map_err(Error::internal)?;
            let mut body = b"001e# service=git-upload-pack\n0000".to_vec();
            body.extend(refs);
            Ok(git_response("application/x-git-upload-pack-advertisement", Body::from(body)))
        }
        (&Method::POST, Route::UploadPack) => {
            ctx.principal.require_repo_read(&ctx.state, &ctx.repo)?;
            ensure(&ctx.state, &ctx.repo, &m, false).await?;
            let gzip =
                ctx.headers.get(header::CONTENT_ENCODING).is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"gzip"));
            let mut input = ctx.body_bytes(MAX_REQUEST).await?;
            if gzip {
                input = gunzip(&input)?;
            }
            let mut c = git(&ctx.state, &ctx.repo);
            c.args(["upload-pack", "--stateless-rpc"]).arg(&m.dir);
            let body = stream(c, Some(input))?;
            Ok(git_response("application/x-git-upload-pack-result", body))
        }
        (&Method::GET | &Method::HEAD, Route::Archive { reference, format }) => {
            ctx.principal.require_repo_read(&ctx.state, &ctx.repo)?;
            let commit = commit_for(&ctx, &m, &reference).await?;
            let safe_ref: String = reference
                .trim_start_matches("refs/tags/")
                .trim_start_matches("refs/heads/")
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '-' })
                .collect();
            let prefix = format!("{}-{}", m.name(), safe_ref.trim_start_matches('v'));
            let filename = format!("{prefix}.{}", format.git_name());
            let body = if ctx.method == Method::HEAD {
                Body::empty()
            } else {
                let mut c = git(&ctx.state, &ctx.repo);
                c.arg("--git-dir")
                    .arg(&m.dir)
                    .args(["archive", &format!("--format={}", format.git_name()), &format!("--prefix={prefix}/")])
                    .arg(&commit);
                stream(c, None)?
            };
            let mut resp = Response::new(body);
            let h = resp.headers_mut();
            h.insert(header::CONTENT_TYPE, HeaderValue::from_static(format.content_type()));
            if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")) {
                h.insert(header::CONTENT_DISPOSITION, v);
            }
            h.insert(header::CACHE_CONTROL, cache_control(&reference, &commit));
            h.insert(header::ETAG, HeaderValue::from_str(&format!("\"{commit}\"")).expect("hex"));
            Ok(resp)
        }
        (&Method::GET | &Method::HEAD, Route::Raw { rest }) => {
            ctx.principal.require_repo_read(&ctx.state, &ctx.repo)?;
            raw_file(&ctx, &m, &rest).await
        }
        (&Method::GET | &Method::HEAD, Route::Repo) => {
            Err(Error::not_found(format!("this is a git proxy: clone {}/{}.git", ctx.repo_url(), m.path)))
        }
        (other, _) => Err(Error::MethodNotAllowed(format!("{other}: git proxies are read-only"))),
    }
}

/// Resolve a ref or commit, refreshing the mirror once if it is unknown.
async fn commit_for(ctx: &Ctx, m: &Mirror, reference: &str) -> Result<String> {
    if !valid_ref(reference) {
        return Err(Error::bad_request(format!("invalid ref '{reference}'")));
    }
    ensure(&ctx.state, &ctx.repo, m, false).await?;
    if let Some(c) = resolve_commit(&ctx.state, &ctx.repo, m, reference).await {
        return Ok(c);
    }
    ensure(&ctx.state, &ctx.repo, m, true).await?;
    resolve_commit(&ctx.state, &ctx.repo, m, reference)
        .await
        .ok_or_else(|| Error::not_found(format!("{}: unknown ref '{reference}'", m.path)))
}

/// `raw/<ref>/<path>`: the ref may contain slashes, so try the shortest
/// prefix that names a commit (as GitHub does).
async fn raw_file(ctx: &Ctx, m: &Mirror, rest: &[String]) -> Result<Response> {
    ensure(&ctx.state, &ctx.repo, m, false).await?;
    let mut found = None;
    for attempt in 0..2 {
        if attempt == 1 {
            ensure(&ctx.state, &ctx.repo, m, true).await?;
        }
        for i in 1..rest.len().min(8) {
            let reference = rest[..i].join("/");
            if let Some(commit) = resolve_commit(&ctx.state, &ctx.repo, m, &reference).await {
                found = Some((reference, commit, rest[i..].join("/")));
                break;
            }
        }
        if found.is_some() {
            break;
        }
    }
    let (reference, commit, file) = found.ok_or_else(|| Error::not_found(format!("{}: unknown ref", m.path)))?;
    let file = trove_core::util::normalize_path(&file)?;
    let object = format!("{commit}:{file}");
    let mut t = git(&ctx.state, &ctx.repo);
    t.arg("--git-dir").arg(&m.dir).args(["cat-file", "-t", &object]);
    match run(t, Duration::from_secs(60)).await {
        Ok(kind) if kind.trim_ascii() == b"blob" => {}
        _ => return Err(Error::not_found(format!("{}: {file} at {reference}", m.path))),
    }
    let body = if ctx.method == Method::HEAD {
        Body::empty()
    } else {
        let mut c = git(&ctx.state, &ctx.repo);
        c.arg("--git-dir").arg(&m.dir).args(["cat-file", "blob", &object]);
        stream(c, None)?
    };
    let mut resp = Response::new(body);
    let ct = crate::content::guess_content_type(&file);
    let h = resp.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&ct) {
        h.insert(header::CONTENT_TYPE, v);
    }
    // Repository files are untrusted: never let a browser run them on Trove's origin.
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("sandbox; default-src 'none'"));
    h.insert(header::CACHE_CONTROL, cache_control(&reference, &commit));
    h.insert(
        header::ETAG,
        HeaderValue::from_str(&format!("\"{commit}:{file}\"")).unwrap_or(HeaderValue::from_static("\"\"")),
    );
    Ok(resp)
}

fn cache_control(reference: &str, commit: &str) -> HeaderValue {
    if reference.eq_ignore_ascii_case(commit) {
        HeaderValue::from_static("public, max-age=31536000, immutable")
    } else {
        HeaderValue::from_static("no-cache")
    }
}

async fn evict(ctx: &Ctx, m: &Mirror) -> Result<Response> {
    ctx.principal.require_admin()?;
    if !m.dir.exists() {
        return Err(Error::not_found(m.path.clone()));
    }
    let key = format!("git:{}:{}", ctx.repo.id, m.path);
    let lock = ctx.state.flight_lock(&key).await;
    let _held = lock.lock().await;
    tokio::fs::remove_dir_all(&m.dir).await.map_err(Error::internal)?;
    audit::record(
        &ctx.state,
        &ctx.principal,
        &ctx.headers,
        "asset.delete",
        format!("{}/{}", ctx.repo.name, m.path),
        "git mirror".to_string(),
    )
    .await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Delete every mirror of a repository (when the repository is deleted).
pub fn remove_mirrors(state: &AppState, repo: &Repository) {
    let dir = state.cfg.git_mirror_dir().join(&repo.id);
    if dir.exists()
        && let Err(e) = std::fs::remove_dir_all(&dir)
    {
        tracing::warn!(dir = %dir.display(), error = %e, "failed to remove git mirrors");
    }
}

fn git_response(content_type: &'static str, body: Body) -> Response {
    let mut resp = Response::new(body);
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache, max-age=0, must-revalidate"));
    resp
}

/// Spawn `cmd` and stream its stdout as the response body.
fn stream(mut cmd: Command, input: Option<Bytes>) -> Result<Body> {
    cmd.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(false);
    let mut child = cmd.spawn().map_err(|e| Error::internal(format!("cannot run git: {e}")))?;
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        tokio::spawn(async move {
            let _ = stdin.write_all(&input).await;
        });
    }
    let stdout = child.stdout.take().ok_or_else(|| Error::internal("git produced no output pipe"))?;
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Ok(Body::from_stream(tokio_util::io::ReaderStream::new(stdout)))
}

fn gunzip(data: &[u8]) -> Result<Bytes> {
    let mut out = Vec::new();
    flate2::read::MultiGzDecoder::new(data)
        .take(MAX_REQUEST as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|e| Error::bad_request(format!("invalid gzip request body: {e}")))?;
    if out.len() > MAX_REQUEST {
        return Err(Error::PayloadTooLarge);
    }
    Ok(out.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(path: &str) -> (String, Route) {
        parse(path).unwrap()
    }

    #[test]
    fn routes() {
        assert_eq!(p("owner/repo.git/info/refs"), ("owner/repo".into(), Route::InfoRefs));
        assert_eq!(p("owner/repo/git-upload-pack"), ("owner/repo".into(), Route::UploadPack));
        assert_eq!(p("owner/repo.git/git-receive-pack").1, Route::ReceivePack);
        assert_eq!(p("owner/repo"), ("owner/repo".into(), Route::Repo));
        assert_eq!(
            p("owner/repo/archive/v1.2.0.tar.gz"),
            ("owner/repo".into(), Route::Archive { reference: "v1.2.0".into(), format: ArchiveFormat::TarGz })
        );
        assert_eq!(
            p("owner/repo/archive/refs/tags/v1.zip").1,
            Route::Archive { reference: "refs/tags/v1".into(), format: ArchiveFormat::Zip }
        );
        assert_eq!(
            p("owner/repo/tar.gz/0123abcd").1,
            Route::Archive { reference: "0123abcd".into(), format: ArchiveFormat::TarGz }
        );
        assert_eq!(
            p("owner/repo/raw/main/docs/a.md").1,
            Route::Raw { rest: vec!["main".into(), "docs".into(), "a.md".into()] }
        );
        assert_eq!(
            p("group/sub/project/-/archive/main/project-main.tar.gz"),
            ("group/sub/project".into(), Route::Archive { reference: "main".into(), format: ArchiveFormat::TarGz })
        );
        assert_eq!(p("group/sub/project.git/info/refs").0, "group/sub/project");
        assert!(parse("owner/repo/blob/main/x").is_err());
        assert!(parse("../etc/info/refs").is_err());
        assert!(parse("owner/.hidden/info/refs").is_err());
    }

    #[test]
    fn refs() {
        assert!(valid_ref("main"));
        assert!(valid_ref("refs/tags/v1.0"));
        assert!(valid_ref("0123456789abcdef0123456789abcdef01234567"));
        assert!(!valid_ref("--upload-pack=evil"));
        assert!(!valid_ref("main..dev"));
        assert!(!valid_ref("HEAD~1"));
        assert!(!valid_ref("a:b"));
    }
}
