use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use serde::Deserialize;
use serde_json::json;
use trove_core::Error;
use trove_core::model::{Role, Token, TokenKind, User};
use trove_core::util::{new_id, now_millis, validate_name};

use crate::audit;
use crate::auth::{self, Principal};
use crate::error::ApiResult;
use crate::state::AppState;

pub async fn list(State(state): State<AppState>, principal: Principal) -> ApiResult<Json<Vec<User>>> {
    principal.require_admin()?;
    Ok(Json(state.db.list_users().await?))
}

#[derive(Deserialize)]
pub struct CreateReq {
    username: String,
    password: String,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    email: String,
    role: Role,
}

pub async fn create(
    State(state): State<AppState>,
    principal: Principal,
    headers: HeaderMap,
    Json(req): Json<CreateReq>,
) -> ApiResult<(StatusCode, Json<User>)> {
    principal.require_admin()?;
    validate_name(&req.username)?;
    auth::validate_password(&req.password)?;
    let user = User {
        id: new_id(),
        username: req.username,
        display_name: req.display_name,
        email: req.email,
        role: req.role,
        disabled: false,
        created_at: now_millis(),
        password_hash: auth::hash_password(&req.password)?,
    };
    state.db.create_user(&user).await?;
    audit::record(&state, &principal, &headers, "user.create", &user.username, format!("role {}", user.role)).await;
    Ok((StatusCode::CREATED, Json(user)))
}

#[derive(Deserialize)]
pub struct UpdateReq {
    display_name: Option<String>,
    email: Option<String>,
    role: Option<Role>,
    disabled: Option<bool>,
    password: Option<String>,
}

pub async fn update(
    State(state): State<AppState>,
    principal: Principal,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<UpdateReq>,
) -> ApiResult<Json<User>> {
    principal.require_admin()?;
    let mut user = state.db.get_user(&id).await?.ok_or_else(|| Error::not_found("user"))?;
    let was_admin = user.role == Role::Admin && !user.disabled;
    if let Some(v) = req.display_name {
        user.display_name = v;
    }
    if let Some(v) = req.email {
        user.email = v;
    }
    if let Some(v) = req.role {
        user.role = v;
    }
    if let Some(v) = req.disabled {
        user.disabled = v;
    }
    if let Some(p) = req.password.filter(|p| !p.is_empty()) {
        auth::validate_password(&p)?;
        user.password_hash = auth::hash_password(&p)?;
    }
    let still_admin = user.role == Role::Admin && !user.disabled;
    if was_admin && !still_admin && state.db.count_admins().await? <= 1 {
        return Err(Error::bad_request("cannot demote or disable the last administrator").into());
    }
    state.db.update_user(&user).await?;
    state.credentials.clear();
    audit::record(&state, &principal, &headers, "user.update", &user.username, "").await;
    Ok(Json(user))
}

pub async fn remove(
    State(state): State<AppState>,
    principal: Principal,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    principal.require_admin()?;
    let user = state.db.get_user(&id).await?.ok_or_else(|| Error::not_found("user"))?;
    if principal.user.as_ref().is_some_and(|u| u.id == user.id) {
        return Err(Error::bad_request("you cannot delete yourself").into());
    }
    if user.role == Role::Admin && !user.disabled && state.db.count_admins().await? <= 1 {
        return Err(Error::bad_request("cannot delete the last administrator").into());
    }
    state.db.delete_user(&user.id).await?;
    state.credentials.clear();
    audit::record(&state, &principal, &headers, "user.delete", &user.username, "").await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_tokens(State(state): State<AppState>, principal: Principal) -> ApiResult<Json<Vec<Token>>> {
    let user = principal.require_user()?;
    Ok(Json(state.db.list_tokens(&user.id, TokenKind::Api).await?))
}

#[derive(Deserialize)]
pub struct TokenReq {
    name: String,
    /// Days until expiry; omitted or 0 means no expiry.
    #[serde(default)]
    expires_in_days: u64,
}

pub async fn create_token(
    State(state): State<AppState>,
    principal: Principal,
    headers: HeaderMap,
    Json(req): Json<TokenReq>,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    let user = principal.require_user()?.clone();
    let name = req.name.trim();
    if name.is_empty() || name.len() > 100 {
        return Err(Error::bad_request("token name must be 1-100 characters").into());
    }
    let ttl = (req.expires_in_days > 0).then(|| Duration::from_secs(req.expires_in_days * 86_400));
    let (token, secret) = auth::issue_token(&state, &user, name, TokenKind::Api, ttl).await?;
    audit::record(&state, &principal, &headers, "token.create", name, "").await;
    Ok((StatusCode::CREATED, Json(json!({ "token": token, "secret": secret }))))
}

pub async fn delete_token(
    State(state): State<AppState>,
    principal: Principal,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let user = principal.require_user()?;
    if !state.db.delete_token(&user.id, &id).await? {
        return Err(Error::not_found("token").into());
    }
    audit::record(&state, &principal, &headers, "token.delete", &id, "").await;
    Ok(StatusCode::NO_CONTENT)
}
