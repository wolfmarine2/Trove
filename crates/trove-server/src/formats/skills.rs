//! Agent-skill marketplace proxy for registries speaking the ClawHub HTTP
//! API: Tencent SkillHub (skillhub.cn, API at `https://lightmake.site`),
//! ClawHub (`https://clawhub.ai`) and compatible self-hosted registries.
//!
//! ```text
//! export CLAWHUB_REGISTRY=http://trove.example.com/repository/skillhub
//! clawhub search pdf
//! clawhub install pdf-parser
//! curl -o pdf.zip "http://trove.example.com/repository/skillhub/api/v1/download?slug=pdf-parser"
//! ```
//!
//! Every `GET` maps onto the same path and query at the remote. Versioned
//! downloads (`api/v1/download?slug=…&version=…`, package artifacts,
//! `.zip`/`.tgz` files) are immutable and cached forever; everything else
//! (search, listings, details, unversioned "latest" downloads) is metadata,
//! refreshed after the repository's max age and served stale while the remote
//! is unreachable. Absolute URLs pointing at the remote inside JSON responses
//! are rewritten to point at Trove.

use axum::http::Method;
use axum::response::Response;
use serde_json::json;
use trove_core::util::normalize_path;
use trove_core::{Error, Result};

use super::Ctx;
use crate::content;
use crate::proxy::{self, FetchOpts};

/// JSON documents larger than this are served without URL rewriting.
const REWRITE_LIMIT: i64 = 16 * 1024 * 1024;

pub async fn handle(ctx: Ctx) -> Result<Response> {
    let path = normalize_path(&ctx.path)?;
    match ctx.method {
        Method::GET | Method::HEAD => {}
        // Admins may evict cached responses.
        Method::DELETE => return super::raw::delete_path(&ctx, &path).await,
        ref other => {
            return Err(Error::MethodNotAllowed(format!("{other}: skills proxies are read-only")));
        }
    }
    ctx.principal.require_repo_read(&ctx.state, &ctx.repo)?;
    let query = canonical_query(ctx.uri.query().unwrap_or(""));
    let (cache_path, metadata) = cache_key(&path, &query)?;
    let remote = ctx.repo.proxy().map(|p| p.remote_url.clone()).unwrap_or_default();
    let mut url = content::join_url(&remote, &content::encode_path(&path));
    if !query.is_empty() {
        url.push('?');
        url.push_str(&query);
    }
    let repo_url = ctx.repo_url();
    let fetched = if ctx.repo.online {
        let opts = FetchOpts {
            metadata,
            keep_content_type: true,
            accept: Some("application/json, */*"),
            ..Default::default()
        };
        proxy::fetch_opts(&ctx.state, &ctx.repo, &cache_path, &url, opts).await?
    } else {
        None
    };
    let Some(asset) = fetched else {
        if path == ".well-known/clawhub.json" {
            // Upstream has no discovery document: describe Trove itself.
            let doc = json!({ "apiBase": repo_url, "registry": repo_url });
            return Ok(content::serve_bytes("application/json", doc.to_string(), &ctx.method, &ctx.headers));
        }
        return Err(Error::not_found(path));
    };
    if asset.content_type.contains("json") && asset.blob.size <= REWRITE_LIMIT {
        let body = content::read_asset(&ctx.state, &asset).await?;
        if let Ok(text) = std::str::from_utf8(&body) {
            let rewritten = rewrite_urls(text, &remote, &repo_url);
            return Ok(content::serve_bytes(&asset.content_type, rewritten, &ctx.method, &ctx.headers));
        }
    }
    content::serve_asset(&ctx.state, &asset, &ctx.method, &ctx.headers).await
}

/// Query string with its pairs sorted, so equivalent requests share a cache entry.
fn canonical_query(raw: &str) -> String {
    let mut pairs: Vec<(String, String)> =
        url::form_urlencoded::parse(raw.as_bytes()).map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
    pairs.sort();
    url::form_urlencoded::Serializer::new(String::new()).extend_pairs(pairs).finish()
}

/// Where a response is cached and whether it is mutable metadata.
fn cache_key(path: &str, query: &str) -> Result<(String, bool)> {
    let param = |name: &str| {
        url::form_urlencoded::parse(query.as_bytes())
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    if path.ends_with("download")
        && path.starts_with("api/")
        && let (Some(slug), Some(version)) = (param("slug"), param("version"))
        && version != "latest"
    {
        // Pinned downloads are immutable; store them under a readable path.
        let key = normalize_path(&format!("{path}/{slug}/{version}"))?;
        return Ok((key, false));
    }
    let immutable = (path.contains("/versions/") && path.ends_with("/download"))
        || [".zip", ".tgz", ".tar.gz"].iter().any(|ext| path.ends_with(ext));
    let key = if query.is_empty() { path.to_string() } else { format!("{path}?{query}") };
    Ok((key, !immutable))
}

/// Point absolute URLs of the remote at this repository (plain and
/// JSON-escaped spellings).
fn rewrite_urls(text: &str, remote: &str, repo_url: &str) -> String {
    let remote = remote.trim_end_matches('/');
    let repo_url = repo_url.trim_end_matches('/');
    if remote.is_empty() {
        return text.to_string();
    }
    text.replace(remote, repo_url).replace(&remote.replace('/', "\\/"), &repo_url.replace('/', "\\/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        let q = canonical_query("version=1.0.0&slug=pdf");
        assert_eq!(q, "slug=pdf&version=1.0.0");
        assert_eq!(cache_key("api/v1/download", &q).unwrap(), ("api/v1/download/pdf/1.0.0".into(), false));
        let q = canonical_query("slug=%40team%2Fpdf&version=2");
        assert_eq!(cache_key("api/v1/download", &q).unwrap().0, "api/v1/download/@team/pdf/2");
        assert!(cache_key("api/v1/download", "slug=..&version=1").is_err());
        let (k, meta) = cache_key("api/v1/download", "slug=pdf").unwrap();
        assert_eq!((k.as_str(), meta), ("api/v1/download?slug=pdf", true));
        assert_eq!(cache_key("api/v1/search", "q=pdf").unwrap(), ("api/v1/search?q=pdf".into(), true));
        assert!(!cache_key("api/v1/packages/x/versions/1.0.0/artifact/download", "").unwrap().1);
        assert!(!cache_key("api/npm/x/-/x-1.0.0.tgz", "").unwrap().1);
    }

    #[test]
    fn rewriting() {
        let text = r#"{"url":"https://lightmake.site/api/v1/download?slug=a","esc":"https:\/\/lightmake.site\/x","other":"https://cdn.example/x"}"#;
        let out = rewrite_urls(text, "https://lightmake.site/", "http://trove/repository/skillhub");
        assert!(out.contains(r#""url":"http://trove/repository/skillhub/api/v1/download?slug=a""#));
        assert!(out.contains(r#""esc":"http:\/\/trove\/repository\/skillhub\/x""#));
        assert!(out.contains("https://cdn.example/x"));
    }
}
