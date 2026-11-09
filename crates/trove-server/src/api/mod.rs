//! Management REST API (`/api/v1`), used by the web UI and automation.

mod assets;
mod repos;
mod session;
mod system;
mod users;

use axum::Router;
use axum::routing::{delete, get, post, put};

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/status", get(system::status))
        .route("/stats", get(system::stats))
        .route("/auth/login", post(session::login))
        .route("/auth/logout", post(session::logout))
        .route("/auth/me", get(session::me))
        .route("/auth/me/password", put(session::change_password))
        .route("/repositories", get(repos::list).post(repos::create))
        .route("/repositories/{name}", get(repos::get_one).put(repos::update).delete(repos::remove))
        .route("/repositories/{name}/browse", get(repos::browse))
        .route("/repositories/{name}/invalidate-cache", post(repos::invalidate_cache))
        .route("/assets", get(assets::search))
        .route("/assets/{id}", get(assets::get_one).delete(assets::remove))
        .route("/users", get(users::list).post(users::create))
        .route("/users/{id}", put(users::update).delete(users::remove))
        .route("/tokens", get(users::list_tokens).post(users::create_token))
        .route("/tokens/{id}", delete(users::delete_token))
        .route("/system/settings", get(system::get_settings).put(system::put_settings))
        .route("/system/gc", post(system::gc))
        .route("/system/audit", get(system::audit))
}
