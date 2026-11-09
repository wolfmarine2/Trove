use axum::http::HeaderMap;
use trove_core::model::AuditEvent;
use trove_core::util::{new_id, now_millis};

use crate::auth::Principal;
use crate::state::AppState;

/// Best-effort client address from proxy headers.
pub fn client_ip(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .or_else(|| headers.get("x-real-ip").and_then(|v| v.to_str().ok()))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Record an audit event. Failures are logged, never propagated: auditing
/// must not break the operation being audited.
pub async fn record(
    state: &AppState,
    principal: &Principal,
    headers: &HeaderMap,
    action: &str,
    target: impl Into<String>,
    detail: impl Into<String>,
) {
    record_as(state, principal.name(), headers, action, target, detail).await
}

pub async fn record_as(
    state: &AppState,
    actor: &str,
    headers: &HeaderMap,
    action: &str,
    target: impl Into<String>,
    detail: impl Into<String>,
) {
    let event = AuditEvent {
        id: new_id(),
        ts: now_millis(),
        actor: actor.to_string(),
        action: action.to_string(),
        target: target.into(),
        detail: detail.into(),
        client_ip: client_ip(headers),
    };
    tracing::info!(target: "trove::audit", actor = %event.actor, action = %event.action, target = %event.target, detail = %event.detail, ip = %event.client_ip);
    if let Err(e) = state.db.insert_audit(&event).await {
        tracing::error!(error = %e, "failed to write audit event");
    }
}
