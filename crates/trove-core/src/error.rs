use thiserror::Error;

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Error type used across Trove. The HTTP layer maps each variant to a status code.
#[derive(Debug, Error)]
pub enum Error {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("authentication required")]
    Unauthorized,
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("method not allowed: {0}")]
    MethodNotAllowed(String),
    #[error("payload too large")]
    PayloadTooLarge,
    #[error("upstream error: {0}")]
    Upstream(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl Error {
    pub fn not_found(what: impl Into<String>) -> Self {
        Self::NotFound(what.into())
    }
    pub fn bad_request(what: impl Into<String>) -> Self {
        Self::BadRequest(what.into())
    }
    pub fn conflict(what: impl Into<String>) -> Self {
        Self::Conflict(what.into())
    }
    pub fn forbidden(what: impl Into<String>) -> Self {
        Self::Forbidden(what.into())
    }
    pub fn internal(what: impl std::fmt::Display) -> Self {
        Self::Internal(anyhow::anyhow!("{what}"))
    }
}

impl From<sqlx::Error> for Error {
    fn from(e: sqlx::Error) -> Self {
        match &e {
            sqlx::Error::RowNotFound => Error::NotFound("record".into()),
            sqlx::Error::Database(db) if db.is_unique_violation() => Error::Conflict("record already exists".into()),
            _ => Error::Internal(e.into()),
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Internal(e.into())
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::BadRequest(format!("invalid json: {e}"))
    }
}
