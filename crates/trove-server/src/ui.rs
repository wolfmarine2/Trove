//! Web UI hosting with single-page-application fallback.
//!
//! The UI is taken from `ui.dir` when configured, otherwise from the copy
//! embedded at build time (`--features embed-ui`), otherwise a minimal
//! placeholder page is shown.

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::response::{Html, IntoResponse, Response};

use crate::state::AppState;

#[cfg(feature = "embed-ui")]
#[derive(rust_embed::RustEmbed)]
#[folder = "../../web/dist"]
struct Embedded;

const PLACEHOLDER: &str = r#"<!DOCTYPE html><html><head><meta charset="utf-8"><title>Trove</title></head>
<body style="font-family:system-ui;margin:4rem auto;max-width:40rem;color:#222">
<h1>Trove</h1><p>The server is running, but this build does not include the web UI.</p>
<p>Build it with <code>cd web &amp;&amp; npm ci &amp;&amp; npm run build</code>, then either set
<code>ui.dir = "web/dist"</code> or compile with <code>--features embed-ui</code>.</p>
<p>API status: <a href="/api/v1/status">/api/v1/status</a></p></body></html>"#;

fn content_type(path: &str) -> String {
    mime_guess::from_path(path).first_or_octet_stream().essence_str().to_string()
}

fn respond(path: &str, data: Vec<u8>) -> Response {
    let mut resp = Response::new(Body::from(data));
    let h = resp.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&content_type(path)) {
        h.insert(header::CONTENT_TYPE, v);
    }
    let cache = if path.starts_with("assets/") { "public, max-age=31536000, immutable" } else { "no-cache" };
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    h.insert("x-frame-options", HeaderValue::from_static("DENY"));
    resp
}

async fn load(state: &AppState, path: &str) -> Option<Vec<u8>> {
    if path.contains("..") {
        return None;
    }
    if !state.cfg.ui.dir.is_empty() {
        let full = std::path::Path::new(&state.cfg.ui.dir).join(path);
        return tokio::fs::read(full).await.ok();
    }
    #[cfg(feature = "embed-ui")]
    {
        return Embedded::get(path).map(|f| f.data.into_owned());
    }
    #[allow(unreachable_code)]
    None
}

pub async fn serve(State(state): State<AppState>, uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    if let Some(data) = load(&state, path).await {
        return respond(path, data);
    }
    // Unknown API or asset paths are real 404s; everything else is a client-side route.
    if path.starts_with("api/") || path.starts_with("assets/") || path.starts_with("repository/") {
        return StatusCode::NOT_FOUND.into_response();
    }
    match load(&state, "index.html").await {
        Some(data) => respond("index.html", data),
        None => Html(PLACEHOLDER).into_response(),
    }
}
