use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use trove_core::Error;

/// Wrapper that renders `trove_core::Error` as an HTTP response.
#[derive(Debug)]
pub struct AppError(pub Error);

pub type ApiResult<T> = std::result::Result<T, AppError>;

impl<E: Into<Error>> From<E> for AppError {
    fn from(e: E) -> Self {
        AppError(e.into())
    }
}

pub fn status_of(e: &Error) -> StatusCode {
    match e {
        Error::NotFound(_) => StatusCode::NOT_FOUND,
        Error::BadRequest(_) => StatusCode::BAD_REQUEST,
        Error::Conflict(_) => StatusCode::CONFLICT,
        Error::Unauthorized => StatusCode::UNAUTHORIZED,
        Error::Forbidden(_) => StatusCode::FORBIDDEN,
        Error::MethodNotAllowed(_) => StatusCode::METHOD_NOT_ALLOWED,
        Error::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
        Error::Upstream(_) => StatusCode::BAD_GATEWAY,
        Error::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

pub fn message_of(e: &Error) -> String {
    match e {
        // Never leak internals to clients.
        Error::Internal(inner) => {
            tracing::error!(error = ?inner, "internal error");
            "internal server error".to_string()
        }
        other => other.to_string(),
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = status_of(&self.0);
        let body = Json(serde_json::json!({ "error": message_of(&self.0) }));
        (status, body).into_response()
    }
}

/// Error renderer for package-manager endpoints: adds a Basic challenge on 401
/// so Maven, pip and friends retry with credentials.
pub fn format_error(e: Error, json: bool) -> Response {
    let status = status_of(&e);
    let msg = message_of(&e);
    let mut resp = if json {
        (status, Json(serde_json::json!({ "error": msg }))).into_response()
    } else {
        (status, format!("{msg}\n")).into_response()
    };
    if status == StatusCode::UNAUTHORIZED {
        resp.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Basic realm=\"Trove\""));
    }
    resp
}
