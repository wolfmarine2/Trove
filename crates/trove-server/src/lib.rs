//! Trove server library: builds the HTTP application from configuration.

pub mod api;
pub mod audit;
pub mod auth;
pub mod content;
pub mod error;
pub mod formats;
pub mod gc;
pub mod metrics;
pub mod proxy;
pub mod state;
pub mod ui;

use std::time::Duration;

use axum::Router;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{any, get};
use tower_http::trace::TraceLayer;
use trove_core::config::Config;

pub use state::AppState;

async fn healthz() -> &'static str {
    "ok"
}

async fn readyz(State(state): State<AppState>) -> impl IntoResponse {
    match state.db.ping().await {
        Ok(()) => (StatusCode::OK, "ready"),
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "database unavailable"),
    }
}

/// Build the complete router for `state`.
pub fn router(state: AppState) -> Router {
    let repository = Router::new()
        .route("/repository/{repo}", any(formats::dispatch_root))
        .route("/repository/{repo}/", any(formats::dispatch_root))
        .route("/repository/{repo}/{*path}", any(formats::dispatch))
        // Size limits are enforced while streaming into the blob store.
        .layer(DefaultBodyLimit::disable());

    Router::new()
        .nest("/api/v1", api::router().layer(DefaultBodyLimit::max(4 * 1024 * 1024)))
        .merge(repository)
        .route("/v2", any(formats::docker::dispatch))
        .route("/v2/", any(formats::docker::dispatch))
        .route("/v2/{*path}", any(formats::docker::dispatch))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics::render))
        .fallback(ui::serve)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Initialise state (database, storage, bootstrap admin) for `cfg`.
pub async fn init(cfg: Config) -> anyhow::Result<AppState> {
    let state = AppState::new(cfg).await?;
    auth::bootstrap_admin(&state).await?;
    Ok(state)
}

/// Run the server until shutdown (SIGINT / SIGTERM).
pub async fn serve(cfg: Config) -> anyhow::Result<()> {
    let bind = cfg.server.bind.clone();
    let state = init(cfg).await?;
    gc::spawn_periodic(state.clone(), Duration::from_secs(6 * 3600));
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        address = %listener.local_addr()?,
        storage = state.blobs.description(),
        "Trove is listening"
    );
    axum::serve(listener, router(state)).with_graceful_shutdown(shutdown_signal()).await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutting down");
}
