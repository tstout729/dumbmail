use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    NotFound(String),
    #[error("sign in required")]
    Unauthorized,
    #[error("{0}")]
    Forbidden(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    LimitReached(String),
    /// Too much running at once; try again shortly (429).
    #[error("{0}")]
    Busy(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

pub type AppResult<T> = Result<T, AppError>;

impl AppError {
    pub fn bad(msg: impl Into<String>) -> Self {
        AppError::BadRequest(msg.into())
    }
    pub fn not_found(what: &str) -> Self {
        AppError::NotFound(format!("{what} not found"))
    }
    pub fn code(&self) -> &'static str {
        match self {
            AppError::BadRequest(_) => "bad_request",
            AppError::NotFound(_) => "not_found",
            AppError::Unauthorized => "unauthorized",
            AppError::Forbidden(_) => "forbidden",
            AppError::Conflict(_) => "conflict",
            AppError::LimitReached(_) => "limit_reached",
            AppError::Busy(_) => "busy",
            AppError::Internal(_) => "internal",
        }
    }
    pub fn status(&self) -> StatusCode {
        match self {
            AppError::BadRequest(_) => StatusCode::BAD_REQUEST,
            AppError::NotFound(_) => StatusCode::NOT_FOUND,
            AppError::Unauthorized => StatusCode::UNAUTHORIZED,
            AppError::Forbidden(_) => StatusCode::FORBIDDEN,
            AppError::Conflict(_) => StatusCode::CONFLICT,
            AppError::LimitReached(_) => StatusCode::PAYMENT_REQUIRED,
            AppError::Busy(_) => StatusCode::TOO_MANY_REQUESTS,
            AppError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
    /// Message safe to show a user (internal errors are not leaked).
    pub fn public_message(&self) -> String {
        match self {
            AppError::Internal(_) => "Something went wrong on our side. Please try again.".into(),
            other => other.to_string(),
        }
    }
}

impl From<sqlx::Error> for AppError {
    fn from(e: sqlx::Error) -> Self {
        match &e {
            sqlx::Error::RowNotFound => AppError::NotFound("not found".into()),
            sqlx::Error::Database(db) if db.code().as_deref() == Some("23505") => {
                AppError::Conflict("that already exists".into())
            }
            sqlx::Error::Database(db) if db.code().as_deref() == Some("22P02") => {
                AppError::BadRequest("invalid id or value".into())
            }
            _ => AppError::Internal(e.into()),
        }
    }
}

impl From<serde_json::Error> for AppError {
    fn from(e: serde_json::Error) -> Self {
        AppError::BadRequest(format!("invalid JSON: {e}"))
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        if let AppError::Internal(e) = &self {
            tracing::error!(error = ?e, "internal error");
        }
        let body = json!({ "error": { "code": self.code(), "message": self.public_message() } });
        (self.status(), Json(body)).into_response()
    }
}
