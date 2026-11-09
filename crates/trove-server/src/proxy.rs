//! Upstream fetching with caching for proxy repositories.
//!
//! * Content files are immutable: once cached they are served forever.
//! * Metadata documents are refreshed after `metadata_max_age_secs`, using
//!   conditional requests (`If-None-Match` / `If-Modified-Since`).
//! * When the upstream is unreachable, stale cached content is served.
//! * Upstream 404s are remembered for a minute to avoid hammering remotes.

use axum::http::{HeaderMap, StatusCode};
use futures::TryStreamExt;
use serde_json::{Value, json};
use trove_core::model::{Asset, DEFAULT_METADATA_MAX_AGE_SECS, Repository};
use trove_core::util::now_millis;
use trove_core::{Error, Result};
use trove_db::NewAsset;

use crate::content::guess_content_type;
use crate::state::{AppState, Metrics};

#[derive(Default, Clone, Copy)]
pub struct FetchOpts<'a> {
    pub metadata: bool,
    pub accept: Option<&'a str>,
    /// Negotiate Docker/OCI registry bearer tokens when challenged.
    pub registry_auth: bool,
    /// Keep the upstream `Content-Type` (e.g. OCI manifest media types).
    pub keep_content_type: bool,
    /// Expected SHA-256 (hex) of the body; a mismatch is rejected, nothing is cached.
    pub expect_sha256: Option<&'a str>,
    /// Treat cached metadata older than this timestamp (ms) as stale, e.g.
    /// because the signed index that describes it has changed.
    pub stale_before: Option<i64>,
}

pub async fn fetch(
    state: &AppState,
    repo: &Repository,
    cache_path: &str,
    url: &str,
    metadata: bool,
) -> Result<Option<Asset>> {
    fetch_opts(state, repo, cache_path, url, FetchOpts { metadata, ..Default::default() }).await
}

/// Render an error with its full source chain (reqwest hides the cause otherwise).
fn chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut cur = e.source();
    while let Some(c) = cur {
        out.push_str(": ");
        out.push_str(&c.to_string());
        cur = c.source();
    }
    out
}

fn is_fresh(repo: &Repository, asset: &Asset, opts: &FetchOpts<'_>) -> bool {
    if !opts.metadata {
        return true;
    }
    let max_age =
        repo.proxy().map(|p| p.metadata_max_age_secs).filter(|v| *v > 0).unwrap_or(DEFAULT_METADATA_MAX_AGE_SECS)
            as i64;
    let cached_at = asset.attributes.get("cached_at").and_then(Value::as_i64).unwrap_or(asset.updated_at);
    let invalidated_at = repo.proxy().map(|p| p.invalidated_at).unwrap_or(0).max(opts.stale_before.unwrap_or(0));
    cached_at > invalidated_at && now_millis() - cached_at < max_age * 1000
}

pub async fn fetch_opts(
    state: &AppState,
    repo: &Repository,
    cache_path: &str,
    url: &str,
    opts: FetchOpts<'_>,
) -> Result<Option<Asset>> {
    let Some(proxy) = repo.proxy() else { return Ok(None) };
    let cached = state.db.get_asset(&repo.id, cache_path).await?;
    if let Some(a) = &cached {
        if is_fresh(repo, a, &opts) {
            Metrics::inc(&state.metrics.proxy_hits);
            return Ok(cached);
        }
    }
    if proxy.offline {
        return Ok(cached);
    }
    let neg_key = format!("{}:{}", repo.id, cache_path);
    if cached.is_none() && state.is_negative(&neg_key).await {
        return Ok(None);
    }

    // Single-flight: whoever gets the lock first downloads; others re-check the cache.
    let lock = state.flight_lock(&neg_key).await;
    let _held = lock.lock().await;
    let cached = state.db.get_asset(&repo.id, cache_path).await?;
    if let Some(a) = &cached {
        if is_fresh(repo, a, &opts) {
            Metrics::inc(&state.metrics.proxy_hits);
            return Ok(cached);
        }
    }

    let conditional = |mut req: reqwest::RequestBuilder| {
        if let Some(accept) = opts.accept {
            req = req.header("accept", accept);
        }
        if let Some(a) = &cached {
            if let Some(etag) = a.attributes.get("etag").and_then(Value::as_str) {
                req = req.header("if-none-match", etag);
            }
            if let Some(lm) = a.attributes.get("last_modified").and_then(Value::as_str) {
                req = req.header("if-modified-since", lm);
            }
        }
        req
    };
    Metrics::inc(&state.metrics.proxy_fetches);
    tracing::debug!(repo = %repo.name, %url, "fetching from upstream");

    let resp = match send_authenticated(state, repo, reqwest::Method::GET, url, opts.registry_auth, &conditional).await
    {
        Ok(r) => r,
        Err(detail) => {
            Metrics::inc(&state.metrics.upstream_errors);
            tracing::warn!(repo = %repo.name, %url, error = %detail, "upstream request failed");
            return match cached {
                Some(a) => Ok(Some(a)),
                None => Err(Error::Upstream(format!("{url}: {detail}"))),
            };
        }
    };

    let status = resp.status();
    if status == reqwest::StatusCode::NOT_MODIFIED {
        Metrics::inc(&state.metrics.proxy_not_modified);
        if let Some(mut a) = cached {
            if let Some(obj) = a.attributes.as_object_mut() {
                obj.insert("cached_at".into(), json!(now_millis()));
            } else {
                a.attributes = json!({ "cached_at": now_millis() });
            }
            state.db.set_asset_attributes(&a.id, &a.attributes).await?;
            return Ok(Some(a));
        }
        return Ok(None);
    }
    if matches!(status.as_u16(), 404 | 410) {
        state.set_negative(&neg_key).await;
        return Ok(None);
    }
    if !status.is_success() {
        Metrics::inc(&state.metrics.upstream_errors);
        tracing::warn!(repo = %repo.name, %url, %status, "upstream returned an error");
        return match cached {
            Some(a) => Ok(Some(a)),
            None if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN => {
                Err(Error::Upstream(format!("upstream denied access ({status})")))
            }
            None => Err(Error::Upstream(format!("{url}: HTTP {status}"))),
        };
    }

    let headers = resp.headers().clone();
    let header_str = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
    let content_type = header_str("content-type")
        .map(|c| c.split(';').next().unwrap_or("").trim().to_string())
        .filter(|c| !c.is_empty() && (opts.keep_content_type || (c != "application/octet-stream" && !opts.metadata)))
        .unwrap_or_else(|| guess_content_type(cache_path));
    let mut attributes = json!({ "cached_at": now_millis(), "upstream_url": url });
    if let Some(etag) = header_str("etag") {
        attributes["etag"] = json!(etag);
    }
    if let Some(lm) = header_str("last-modified") {
        attributes["last_modified"] = json!(lm);
    }

    let _gc = state.gc_lock.read().await;
    let stream = resp.bytes_stream().map_err(std::io::Error::other);
    let blob = match state.blobs.ingest(stream, state.max_upload()).await {
        Ok(b) => b,
        Err(e) => {
            Metrics::inc(&state.metrics.upstream_errors);
            tracing::warn!(repo = %repo.name, %url, error = %e, "failed to download from upstream");
            return match cached {
                Some(a) => Ok(Some(a)),
                None => Err(Error::Upstream(format!("{url}: {e}"))),
            };
        }
    };
    if let Some(expected) = opts.expect_sha256 {
        if !blob.sha256.eq_ignore_ascii_case(expected) {
            Metrics::inc(&state.metrics.upstream_errors);
            tracing::warn!(repo = %repo.name, %url, expected, actual = %blob.sha256, "upstream content failed digest check");
            return Err(Error::Upstream(format!("{url}: digest mismatch (expected sha256:{expected})")));
        }
    }
    let (asset, _) = state
        .db
        .put_asset(NewAsset {
            repo_id: &repo.id,
            path: cache_path,
            blob: &blob,
            content_type: &content_type,
            uploader: "proxy",
            attributes,
        })
        .await?;
    Ok(Some(asset))
}

/// Parse `Bearer realm="…",service="…",scope="…"` into key/value pairs.
pub fn parse_challenge(header: &str) -> Option<(String, Vec<(String, String)>)> {
    let (scheme, rest) = header.trim().split_once(' ')?;
    let mut params = Vec::new();
    let mut rest = rest.trim();
    while !rest.is_empty() {
        let (key, after) = rest.split_once('=')?;
        let after = after.trim_start();
        let (value, remainder) = if let Some(quoted) = after.strip_prefix('"') {
            let end = quoted.find('"')?;
            (&quoted[..end], &quoted[end + 1..])
        } else {
            let end = after.find(',').unwrap_or(after.len());
            (&after[..end], &after[end..])
        };
        params.push((key.trim().to_ascii_lowercase(), value.to_string()));
        rest = remainder.trim_start().trim_start_matches(',').trim_start();
    }
    Some((scheme.to_ascii_lowercase(), params))
}

/// Obtain a registry bearer token for the challenge, using the repository's
/// upstream credentials when configured. Tokens are cached until expiry.
async fn registry_token(
    state: &AppState,
    repo: &Repository,
    challenge: &[(String, String)],
) -> std::result::Result<String, String> {
    let get = |k: &str| challenge.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());
    let realm = get("realm").ok_or("bearer challenge without realm")?;
    let cache_key = format!("{}|{}|{}", repo.id, get("service").unwrap_or_default(), get("scope").unwrap_or_default());
    if let Some(token) = state.cached_registry_token(&cache_key).await {
        return Ok(token);
    }
    let mut url = url::Url::parse(&realm).map_err(|e| format!("invalid token realm {realm}: {e}"))?;
    {
        let mut q = url.query_pairs_mut();
        for k in ["service", "scope"] {
            if let Some(v) = get(k) {
                q.append_pair(k, &v);
            }
        }
    }
    let mut req = state.http.get(url.as_str());
    if let Some(p) = repo.proxy().filter(|p| !p.username.is_empty()) {
        req = req.basic_auth(&p.username, Some(&p.password));
    }
    let resp = req.send().await.map_err(|e| chain(&e))?;
    if !resp.status().is_success() {
        return Err(format!("token endpoint {realm} returned {}", resp.status()));
    }
    let body: Value = resp.json().await.map_err(|e| chain(&e))?;
    let token = body
        .get("token")
        .or_else(|| body.get("access_token"))
        .and_then(Value::as_str)
        .ok_or("token response without token")?
        .to_string();
    let ttl = body.get("expires_in").and_then(Value::as_u64).unwrap_or(60).clamp(30, 3600);
    // Refresh a little early so in-flight requests do not race expiry.
    state.cache_registry_token(&cache_key, &token, std::time::Duration::from_secs(ttl.saturating_sub(10))).await;
    Ok(token)
}

/// Send a request upstream, handling basic credentials and (optionally)
/// the Docker registry token dance: try with a cached token or anonymously,
/// and on a `401 Bearer` challenge fetch a token and retry once.
pub async fn send_authenticated(
    state: &AppState,
    repo: &Repository,
    method: reqwest::Method,
    url: &str,
    registry_auth: bool,
    decorate: &(dyn Fn(reqwest::RequestBuilder) -> reqwest::RequestBuilder + Sync),
) -> std::result::Result<reqwest::Response, String> {
    let proxy = repo.proxy().cloned().unwrap_or_default();
    let has_creds = !proxy.username.is_empty();
    let build = |auth: Option<&str>| {
        let mut req = decorate(state.http.request(method.clone(), url));
        match auth {
            Some(token) => req = req.bearer_auth(token),
            None if has_creds && !registry_auth => req = req.basic_auth(&proxy.username, Some(&proxy.password)),
            None => {}
        }
        req
    };
    let cached = if registry_auth { state.cached_registry_token(&format!("{}|last", repo.id)).await } else { None };
    let resp = build(cached.as_deref()).send().await.map_err(|e| chain(&e))?;
    if !registry_auth || resp.status() != reqwest::StatusCode::UNAUTHORIZED {
        return Ok(resp);
    }
    let header = resp.headers().get("www-authenticate").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
    match parse_challenge(&header) {
        Some((scheme, params)) if scheme == "bearer" => {
            let token = registry_token(state, repo, &params).await?;
            let resp = build(Some(&token)).send().await.map_err(|e| chain(&e))?;
            if resp.status().is_success() {
                // Remember the last good token: most requests share one scope.
                state
                    .cache_registry_token(&format!("{}|last", repo.id), &token, std::time::Duration::from_secs(50))
                    .await;
            }
            Ok(resp)
        }
        Some((scheme, _)) if scheme == "basic" && has_creds => {
            let req =
                decorate(state.http.request(method.clone(), url)).basic_auth(&proxy.username, Some(&proxy.password));
            req.send().await.map_err(|e| chain(&e))
        }
        _ => Ok(resp),
    }
}

/// Forward a request verbatim to the upstream of a proxy repository and
/// return the response without caching (used for e.g. `npm audit`).
pub async fn forward(
    state: &AppState,
    repo: &Repository,
    method: reqwest::Method,
    url: &str,
    headers: &HeaderMap,
    body: bytes::Bytes,
) -> Result<axum::response::Response> {
    let Some(proxy) = repo.proxy() else { return Err(Error::not_found("no upstream")) };
    let mut req = state.http.request(method, url).body(body);
    for name in ["content-type", "accept", "content-encoding", "npm-command"] {
        if let Some(v) = headers.get(name) {
            req = req.header(name, v.as_bytes());
        }
    }
    if !proxy.username.is_empty() {
        req = req.basic_auth(&proxy.username, Some(&proxy.password));
    }
    let resp = req.send().await.map_err(|e| Error::Upstream(e.to_string()))?;
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let ct = resp.headers().get("content-type").cloned();
    let enc = resp.headers().get("content-encoding").cloned();
    let bytes = resp.bytes().await.map_err(|e| Error::Upstream(e.to_string()))?;
    let mut out = axum::response::Response::new(axum::body::Body::from(bytes));
    *out.status_mut() = status;
    if let Some(ct) = ct.and_then(|v| axum::http::HeaderValue::from_bytes(v.as_bytes()).ok()) {
        out.headers_mut().insert(axum::http::header::CONTENT_TYPE, ct);
    }
    if let Some(enc) = enc.and_then(|v| axum::http::HeaderValue::from_bytes(v.as_bytes()).ok()) {
        out.headers_mut().insert(axum::http::header::CONTENT_ENCODING, enc);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_parsing() {
        let (scheme, p) = parse_challenge(
            r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/nginx:pull""#,
        )
        .unwrap();
        assert_eq!(scheme, "bearer");
        assert_eq!(p[0], ("realm".into(), "https://auth.docker.io/token".into()));
        assert_eq!(p[2].1, "repository:library/nginx:pull");
        let (scheme, p) = parse_challenge("Basic realm=Registry").unwrap();
        assert_eq!(scheme, "basic");
        assert_eq!(p[0].1, "Registry");
    }
}
