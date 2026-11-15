//! Go module proxy (the `GOPROXY` protocol), proxy and group repositories,
//! like Nexus' go-proxy and go-group.
//!
//! ```sh
//! go env -w GOPROXY=https://trove.example.com/repository/go/
//! ```
//!
//! Paths below the repository root (module paths and versions are in the
//! go command's case-encoded form, e.g. `github.com/!azure/…`):
//!
//! * `<module>/@v/list`              — known versions (metadata)
//! * `<module>/@latest`              — latest version info (metadata)
//! * `<module>/@v/<version>.info`    — version info; immutable for canonical
//!   versions, metadata for queries such as `master` or a commit hash
//! * `<module>/@v/<version>.mod`     — go.mod (immutable)
//! * `<module>/@v/<version>.zip`     — module source (immutable)
//! * `sumdb/<sumdb-name>/…`          — checksum database, passed through to the
//!   upstream proxy so `go` can verify `go.sum` without reaching
//!   sum.golang.org directly. `supported` and `latest` are metadata; lookups
//!   and tiles are immutable.
//!
//! Groups answer from the first member that has the file, except `@v/list`,
//! which is the union of all members' versions.

use std::collections::BTreeSet;

use axum::http::Method;
use axum::response::Response;
use trove_core::model::{Asset, RepoKind, Repository};
use trove_core::util::normalize_path;
use trove_core::{Error, Result};

use super::Ctx;
use crate::content;
use crate::proxy;
use crate::state::AppState;

/// What a request path asks for.
#[derive(Debug, PartialEq, Eq)]
enum Target<'a> {
    List(&'a str),
    Latest(&'a str),
    Info(&'a str, &'a str),
    Mod(&'a str, &'a str),
    Zip(&'a str, &'a str),
    Sumdb(&'a str),
}

/// Module path in its case-encoded form: lowercase letters, digits and
/// `-._~!`, slash-separated, no empty segments.
fn valid_module(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 1024
        && path.split('/').all(|seg| {
            !seg.is_empty()
                && !seg.starts_with('.')
                && seg.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-._~!".contains(&b))
        })
}

/// Version or version query in its case-encoded form.
fn valid_version(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 256
        && !v.starts_with('.')
        && v.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-._~!+".contains(&b))
}

fn valid_sumdb(rest: &str) -> bool {
    let mut segs = rest.split('/');
    // The first segment names the checksum database host, e.g. sum.golang.org.
    let host = segs.next().unwrap_or_default();
    !host.is_empty()
        && host.contains('.')
        && host.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-.".contains(&b))
        && segs.next().is_some()
}

fn parse_target(path: &str) -> Option<Target<'_>> {
    if let Some(rest) = path.strip_prefix("sumdb/") {
        return valid_sumdb(rest).then_some(Target::Sumdb(rest));
    }
    if let Some(module) = path.strip_suffix("/@latest") {
        return valid_module(module).then_some(Target::Latest(module));
    }
    let (module, file) = path.rsplit_once("/@v/")?;
    if !valid_module(module) {
        return None;
    }
    if file == "list" {
        return Some(Target::List(module));
    }
    let (version, ext) = file.rsplit_once('.')?;
    if !valid_version(version) {
        return None;
    }
    match ext {
        "info" => Some(Target::Info(module, version)),
        "mod" => Some(Target::Mod(module, version)),
        "zip" => Some(Target::Zip(module, version)),
        _ => None,
    }
}

/// Whether `v` is a canonical semantic version as the go command uses them
/// (`vMAJOR.MINOR.PATCH[-pre][+incompatible]`, pseudo-versions included).
/// Only those name a fixed module version; anything else is a query whose
/// answer changes over time.
fn is_canonical(v: &str) -> bool {
    let Some(rest) = v.strip_prefix('v') else { return false };
    let rest = rest.strip_suffix("+incompatible").unwrap_or(rest);
    let (core, pre) = match rest.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (rest, None),
    };
    let nums: Vec<&str> = core.split('.').collect();
    let numeric =
        |s: &&str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) && (s.len() == 1 || !s.starts_with('0'));
    nums.len() == 3
        && nums.iter().all(numeric)
        && pre.is_none_or(|p| {
            p.split('.').all(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
        })
}

/// Mutable documents are refreshed after the repository's metadata max age.
fn is_metadata(target: &Target<'_>) -> bool {
    match target {
        Target::List(_) | Target::Latest(_) => true,
        Target::Info(_, v) | Target::Mod(_, v) | Target::Zip(_, v) => !is_canonical(v),
        Target::Sumdb(rest) => rest.ends_with("/supported") || rest.ends_with("/latest"),
    }
}

pub async fn handle(ctx: Ctx) -> Result<Response> {
    let path = normalize_path(&ctx.path)?;
    match ctx.method {
        Method::GET | Method::HEAD => {
            ctx.principal.require_repo_read(&ctx.state, &ctx.repo)?;
            let target =
                parse_target(&path).ok_or_else(|| Error::not_found(format!("not a Go module path: {path}")))?;
            if let Target::List(module) = target
                && ctx.repo.kind == RepoKind::Group
            {
                let versions = list_versions(&ctx.state, &ctx.repo, module, 0)
                    .await?
                    .ok_or_else(|| Error::not_found(format!("unknown module: {module}")))?;
                let body: String = versions.into_iter().map(|v| v + "\n").collect();
                return Ok(content::serve_bytes("text/plain; charset=utf-8", body, &ctx.method, &ctx.headers));
            }
            let asset = fetch(&ctx.state, &ctx.repo, &path, 0).await?.ok_or_else(|| Error::not_found(path.clone()))?;
            content::serve_asset(&ctx.state, &asset, &ctx.method, &ctx.headers).await
        }
        // Admins may evict cached files.
        Method::DELETE => super::raw::delete_path(&ctx, &path).await,
        ref other => Err(Error::MethodNotAllowed(format!("{other}: Go proxy and group repositories are read-only"))),
    }
}

/// Resolve a GOPROXY path: proxy → cached upstream fetch, group → first member that has it.
fn fetch<'a>(
    state: &'a AppState,
    repo: &'a Repository,
    path: &'a str,
    depth: usize,
) -> futures::future::BoxFuture<'a, Result<Option<Asset>>> {
    Box::pin(async move {
        if !repo.online {
            return Ok(None);
        }
        match repo.kind {
            RepoKind::Proxy => {
                let Some(target) = parse_target(path) else { return Ok(None) };
                let remote = repo.proxy().map(|p| p.remote_url.clone()).unwrap_or_default();
                let url = content::join_url(&remote, &content::encode_path(path));
                proxy::fetch(state, repo, path, &url, is_metadata(&target)).await
            }
            RepoKind::Group => {
                let mut failure = None;
                for m in content::group_members(state, repo, depth).await? {
                    match fetch(state, &m, path, depth + 1).await {
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

/// Versions of `module` known to `repo`; `None` when no member knows the module.
fn list_versions<'a>(
    state: &'a AppState,
    repo: &'a Repository,
    module: &'a str,
    depth: usize,
) -> futures::future::BoxFuture<'a, Result<Option<BTreeSet<String>>>> {
    Box::pin(async move {
        if !repo.online {
            return Ok(None);
        }
        match repo.kind {
            RepoKind::Proxy => {
                let path = format!("{module}/@v/list");
                let Some(asset) = fetch(state, repo, &path, depth).await? else { return Ok(None) };
                let data = content::read_asset(state, &asset).await?;
                let text = String::from_utf8_lossy(&data);
                Ok(Some(text.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect()))
            }
            RepoKind::Group => {
                let (mut merged, mut found, mut failure) = (BTreeSet::new(), false, None);
                for m in content::group_members(state, repo, depth).await? {
                    match list_versions(state, &m, module, depth + 1).await {
                        Ok(Some(versions)) => {
                            found = true;
                            merged.extend(versions);
                        }
                        Ok(None) => {}
                        Err(e) => failure = Some(content::member_failed(&m, e)),
                    }
                }
                match (found, failure) {
                    (true, _) => Ok(Some(merged)),
                    (false, Some(e)) => Err(e),
                    (false, None) => Ok(None),
                }
            }
            RepoKind::Hosted => Ok(None),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets() {
        assert_eq!(parse_target("golang.org/x/text/@v/list"), Some(Target::List("golang.org/x/text")));
        assert_eq!(parse_target("golang.org/x/text/@latest"), Some(Target::Latest("golang.org/x/text")));
        assert_eq!(
            parse_target("github.com/!azure/azure-sdk-for-go/@v/v68.0.0+incompatible.info"),
            Some(Target::Info("github.com/!azure/azure-sdk-for-go", "v68.0.0+incompatible"))
        );
        assert_eq!(parse_target("rsc.io/quote/@v/v1.5.2.mod"), Some(Target::Mod("rsc.io/quote", "v1.5.2")));
        assert_eq!(parse_target("rsc.io/quote/@v/v1.5.2.zip"), Some(Target::Zip("rsc.io/quote", "v1.5.2")));
        assert_eq!(
            parse_target("sumdb/sum.golang.org/lookup/rsc.io/quote@v1.5.2"),
            Some(Target::Sumdb("sum.golang.org/lookup/rsc.io/quote@v1.5.2"))
        );
        assert_eq!(parse_target("sumdb/sum.golang.org/supported"), Some(Target::Sumdb("sum.golang.org/supported")));
        // Upper case is never sent by the go command (it is !-escaped).
        assert_eq!(parse_target("github.com/Azure/x/@v/list"), None);
        assert_eq!(parse_target("rsc.io/quote/@v/v1.5.2.tar"), None);
        assert_eq!(parse_target("rsc.io/quote"), None);
        assert_eq!(parse_target("@v/list"), None);
        assert_eq!(parse_target("sumdb/supported"), None);
    }

    #[test]
    fn canonical_versions() {
        for v in ["v1.5.2", "v0.0.0-20191109021931-daa7c04131f5", "v2.0.0+incompatible", "v1.0.0-rc.1", "v10.0.0"] {
            assert!(is_canonical(v), "{v}");
        }
        for v in ["master", "v1.5", "1.5.2", "v1.05.2", "daa7c04131f5", "v1.0.0-", "latest"] {
            assert!(!is_canonical(v), "{v}");
        }
    }

    #[test]
    fn cache_policy() {
        assert!(is_metadata(&Target::List("m")));
        assert!(is_metadata(&Target::Latest("m")));
        assert!(!is_metadata(&Target::Zip("m", "v1.0.0")));
        assert!(!is_metadata(&Target::Info("m", "v1.0.0")));
        assert!(is_metadata(&Target::Info("m", "master")));
        assert!(is_metadata(&Target::Sumdb("sum.golang.org/supported")));
        assert!(is_metadata(&Target::Sumdb("sum.golang.org/latest")));
        assert!(!is_metadata(&Target::Sumdb("sum.golang.org/lookup/rsc.io/quote@v1.5.2")));
        assert!(!is_metadata(&Target::Sumdb("sum.golang.org/tile/8/0/001")));
    }
}
