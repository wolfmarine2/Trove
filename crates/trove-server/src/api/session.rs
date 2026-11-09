use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use trove_core::Error;
use trove_core::model::TokenKind;

use crate::audit;
use crate::auth::{self, Principal, SESSION_COOKIE};
use crate::error::ApiResult;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct LoginReq {
    username: String,
    password: String,
}

fn secure_cookie(headers: &HeaderMap, state: &AppState) -> bool {
    state.cfg.server.base_url.starts_with("https://")
        || headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()) == Some("https")
}

pub async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<LoginReq>,
) -> ApiResult<Response> {
    let user = match auth::check_password(&state, &req.username, &req.password).await {
        Ok(u) => u,
        Err(e) => {
            audit::record_as(&state, &req.username, &headers, "auth.login_failed", "ui", "").await;
            return Err(e.into());
        }
    };
    let ttl = Duration::from_secs(state.cfg.security.session_ttl_hours.max(1) * 3600);
    let (_, secret) = auth::issue_token(&state, &user, "web session", TokenKind::Session, Some(ttl)).await?;
    audit::record_as(&state, &user.username, &headers, "auth.login", "ui", "").await;
    let cookie = format!(
        "{SESSION_COOKIE}={secret}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}{}",
        ttl.as_secs(),
        if secure_cookie(&headers, &state) { "; Secure" } else { "" }
    );
    let mut resp = Json(json!({ "user": user })).into_response();
    resp.headers_mut().insert(header::SET_COOKIE, HeaderValue::from_str(&cookie).map_err(Error::internal)?);
    Ok(resp)
}

pub async fn logout(State(state): State<AppState>, principal: Principal) -> ApiResult<Response> {
    if let Some(hash) = &principal.token_hash
        && principal.method == auth::AuthMethod::Session
    {
        state.db.delete_token_by_hash(hash).await?;
    }
    let mut resp = StatusCode::NO_CONTENT.into_response();
    resp.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static("trove_session=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0"),
    );
    Ok(resp)
}

pub async fn me(State(state): State<AppState>, principal: Principal) -> ApiResult<Json<serde_json::Value>> {
    let user = principal.require_user()?;
    Ok(Json(json!({ "user": user, "anonymous_read": state.anonymous_read() })))
}

#[derive(Deserialize)]
pub struct PasswordReq {
    current: String,
    new: String,
}

pub async fn change_password(
    State(state): State<AppState>,
    principal: Principal,
    headers: HeaderMap,
    Json(req): Json<PasswordReq>,
) -> ApiResult<StatusCode> {
    let user = principal.require_user()?;
    let mut fresh = state.db.get_user(&user.id).await?.ok_or(Error::Unauthorized)?;
    if !auth::verify_password(&req.current, &fresh.password_hash) {
        return Err(Error::bad_request("current password is incorrect").into());
    }
    auth::validate_password(&req.new)?;
    fresh.password_hash = auth::hash_password(&req.new)?;
    state.db.update_user(&fresh).await?;
    state.credentials.clear();
    audit::record(&state, &principal, &headers, "user.password_change", &fresh.username, "").await;
    Ok(StatusCode::NO_CONTENT)
}
