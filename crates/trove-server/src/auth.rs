//! Authentication and authorisation.
//!
//! Supported credentials, checked in this order:
//! * `Authorization: Bearer trv_...` — API / session token (npm, UI, scripts)
//! * `Authorization: trv_...` — raw token, as sent by Cargo
//! * `Authorization: Basic base64(user:password)` — password *or* API token as
//!   the password (Maven, pip/twine, curl)
//! * `trove_session` cookie — web UI sessions

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use argon2::{Argon2, PasswordHasher, PasswordVerifier};
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{HeaderMap, Method, header};
use base64::Engine;
use trove_core::model::{Role, Token, TokenKind, User};
use trove_core::util::{new_id, now_millis, random_secret, sha256_hex};
use trove_core::{Error, Result};

use crate::error::AppError;
use crate::state::{AppState, Metrics};

pub const SESSION_COOKIE: &str = "trove_session";
pub const TOKEN_PREFIX: &str = "trv_";
/// Header the web UI sends on mutating requests; cookies alone are not
/// accepted for writes (CSRF protection).
pub const CSRF_HEADER: &str = "x-trove-request";

pub fn hash_password(password: &str) -> Result<String> {
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|h| h.to_string())
        .map_err(|e| Error::internal(format!("hashing password: {e}")))
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    Argon2::default().verify_password(password.as_bytes(), hash).is_ok()
}

pub fn validate_password(password: &str) -> Result<()> {
    if password.chars().count() < 8 {
        return Err(Error::bad_request("password must be at least 8 characters"));
    }
    Ok(())
}

/// Caches successful Basic-auth verifications for a few minutes: Argon2 is
/// intentionally slow and build tools send credentials on every request.
#[derive(Default)]
pub struct CredentialCache {
    entries: Mutex<HashMap<String, (String, Instant)>>,
}

const CRED_TTL: Duration = Duration::from_secs(300);

impl CredentialCache {
    fn key(username: &str, password: &str) -> String {
        sha256_hex(format!("{username}\0{password}"))
    }
    fn get(&self, username: &str, password: &str) -> Option<String> {
        let map = self.entries.lock().ok()?;
        map.get(&Self::key(username, password)).filter(|(_, t)| t.elapsed() < CRED_TTL).map(|(id, _)| id.clone())
    }
    fn put(&self, username: &str, password: &str, user_id: &str) {
        if let Ok(mut map) = self.entries.lock() {
            if map.len() > 10_000 {
                map.retain(|_, (_, t)| t.elapsed() < CRED_TTL);
            }
            map.insert(Self::key(username, password), (user_id.to_string(), Instant::now()));
        }
    }
    /// Drop all cached credentials (after password / role changes).
    pub fn clear(&self) {
        if let Ok(mut map) = self.entries.lock() {
            map.clear();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMethod {
    Anonymous,
    Password,
    Token,
    Session,
}

/// The authenticated caller of a request.
#[derive(Debug, Clone)]
pub struct Principal {
    pub user: Option<User>,
    pub method: AuthMethod,
    /// Hash of the token used, if any (for logout).
    pub token_hash: Option<String>,
}

impl Principal {
    pub fn anonymous() -> Self {
        Self { user: None, method: AuthMethod::Anonymous, token_hash: None }
    }

    pub fn name(&self) -> &str {
        self.user.as_ref().map(|u| u.username.as_str()).unwrap_or("anonymous")
    }

    pub fn role(&self) -> Option<Role> {
        self.user.as_ref().map(|u| u.role)
    }

    pub fn require_user(&self) -> Result<&User> {
        self.user.as_ref().ok_or(Error::Unauthorized)
    }

    pub fn require_read(&self, state: &AppState) -> Result<()> {
        if self.user.is_some() || state.anonymous_read() { Ok(()) } else { Err(Error::Unauthorized) }
    }

    /// Whether this caller may read `repo` (private repositories need a user).
    pub fn can_read_repo(&self, state: &AppState, repo: &trove_core::model::Repository) -> bool {
        self.user.is_some() || (state.anonymous_read() && !repo.config.private)
    }

    pub fn require_repo_read(&self, state: &AppState, repo: &trove_core::model::Repository) -> Result<()> {
        if self.can_read_repo(state, repo) { Ok(()) } else { Err(Error::Unauthorized) }
    }

    pub fn require_deploy(&self) -> Result<()> {
        match self.role() {
            None => Err(Error::Unauthorized),
            Some(r) if r.can_deploy() => Ok(()),
            Some(_) => Err(Error::forbidden("deploy permission required")),
        }
    }

    pub fn require_admin(&self) -> Result<()> {
        match self.role() {
            None => Err(Error::Unauthorized),
            Some(r) if r.is_admin() => Ok(()),
            Some(_) => Err(Error::forbidden("administrator permission required")),
        }
    }
}

fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get_all(header::COOKIE).iter().filter_map(|v| v.to_str().ok()).find_map(|v| {
        v.split(';').map(str::trim).find_map(|kv| {
            let (k, val) = kv.split_once('=')?;
            (k == name).then_some(val)
        })
    })
}

async fn by_token(state: &AppState, secret: &str, method: AuthMethod) -> Result<Principal> {
    let hash = sha256_hex(secret);
    match state.db.find_token(&hash).await? {
        Some((_, user)) if !user.disabled => Ok(Principal { user: Some(user), method, token_hash: Some(hash) }),
        _ => {
            Metrics::inc(&state.metrics.auth_failures);
            Err(Error::Unauthorized)
        }
    }
}

/// Authenticate username + password (password may also be an API token).
pub async fn check_password(state: &AppState, username: &str, password: &str) -> Result<User> {
    if password.starts_with(TOKEN_PREFIX) {
        let p = by_token(state, password, AuthMethod::Token).await?;
        return p.user.ok_or(Error::Unauthorized);
    }
    if let Some(id) = state.credentials.get(username, password)
        && let Some(u) = state.db.get_user(&id).await?.filter(|u| !u.disabled)
    {
        return Ok(u);
    }
    let user = state.db.get_user_by_name(username).await?;
    let ok = match &user {
        Some(u) if !u.disabled => {
            let (pw, hash) = (password.to_string(), u.password_hash.clone());
            tokio::task::spawn_blocking(move || verify_password(&pw, &hash)).await.unwrap_or(false)
        }
        _ => false,
    };
    match (ok, user) {
        (true, Some(u)) => {
            state.credentials.put(username, password, &u.id);
            Ok(u)
        }
        _ => {
            Metrics::inc(&state.metrics.auth_failures);
            Err(Error::Unauthorized)
        }
    }
}

/// Resolve the caller from request headers. Missing credentials yield an
/// anonymous principal; *invalid* credentials are an error.
pub async fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<Principal> {
    if let Some(value) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        let value = value.trim();
        if let Some(token) = value.strip_prefix("Bearer ").or_else(|| value.strip_prefix("bearer ")) {
            return by_token(state, token.trim(), AuthMethod::Token).await;
        }
        if value.starts_with(TOKEN_PREFIX) {
            return by_token(state, value, AuthMethod::Token).await;
        }
        if let Some(encoded) = value.strip_prefix("Basic ").or_else(|| value.strip_prefix("basic ")) {
            let decoded =
                base64::engine::general_purpose::STANDARD.decode(encoded.trim()).map_err(|_| Error::Unauthorized)?;
            let decoded = String::from_utf8(decoded).map_err(|_| Error::Unauthorized)?;
            let (user, pass) = decoded.split_once(':').ok_or(Error::Unauthorized)?;
            let user = check_password(state, user, pass).await?;
            return Ok(Principal { user: Some(user), method: AuthMethod::Password, token_hash: None });
        }
        return Err(Error::Unauthorized);
    }
    if let Some(session) = cookie_value(headers, SESSION_COOKIE) {
        // A stale cookie should not lock the browser out of anonymous pages.
        return Ok(by_token(state, session, AuthMethod::Session).await.unwrap_or_else(|_| Principal::anonymous()));
    }
    Ok(Principal::anonymous())
}

/// Create a token for `user`, returning the plaintext secret (shown once).
pub async fn issue_token(
    state: &AppState,
    user: &User,
    name: &str,
    kind: TokenKind,
    ttl: Option<Duration>,
) -> Result<(Token, String)> {
    let secret = random_secret(TOKEN_PREFIX, 40);
    let token = Token {
        id: new_id(),
        user_id: user.id.clone(),
        name: name.to_string(),
        kind,
        prefix: secret[..TOKEN_PREFIX.len() + 6].to_string(),
        created_at: now_millis(),
        expires_at: ttl.map(|d| now_millis() + d.as_millis() as i64),
        last_used_at: None,
    };
    state.db.create_token(&token, &sha256_hex(&secret)).await?;
    Ok((token, secret))
}

/// Create the bootstrap administrator on first start.
pub async fn bootstrap_admin(state: &AppState) -> anyhow::Result<()> {
    if state.db.count_users().await? > 0 {
        return Ok(());
    }
    let configured = state.cfg.security.initial_admin_password.clone();
    let password = if configured.is_empty() { random_secret("", 24) } else { configured.clone() };
    let user = User {
        id: new_id(),
        username: "admin".into(),
        display_name: "Administrator".into(),
        email: String::new(),
        role: Role::Admin,
        disabled: false,
        created_at: now_millis(),
        password_hash: hash_password(&password)?,
    };
    state.db.create_user(&user).await?;
    if configured.is_empty() {
        let path = state.cfg.server.data_dir.join("admin.password");
        std::fs::write(&path, format!("{password}\n"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        tracing::warn!(
            "created initial user 'admin'; its generated password is in {} — change it and delete the file",
            path.display()
        );
    } else {
        tracing::info!("created initial user 'admin' with the configured password");
    }
    Ok(())
}

impl FromRequestParts<AppState> for Principal {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        let principal = authenticate(state, &parts.headers).await?;
        let safe = matches!(parts.method, Method::GET | Method::HEAD | Method::OPTIONS);
        if principal.method == AuthMethod::Session && !safe && !parts.headers.contains_key(CSRF_HEADER) {
            return Err(AppError(Error::forbidden("missing X-Trove-Request header")));
        }
        Ok(principal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_roundtrip() {
        let h = hash_password("correct horse").unwrap();
        assert!(verify_password("correct horse", &h));
        assert!(!verify_password("wrong", &h));
    }

    #[test]
    fn cookie_parsing() {
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, "a=1; trove_session=trv_abc; b=2".parse().unwrap());
        assert_eq!(cookie_value(&h, SESSION_COOKIE), Some("trv_abc"));
        assert_eq!(cookie_value(&h, "zzz"), None);
    }
}
