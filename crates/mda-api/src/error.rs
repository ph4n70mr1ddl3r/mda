//! Map canonical [`mda_core::Error`] to HTTP responses.
//!
//! A newtype (`ApiError`) is required because of the orphan rules: neither
//! `IntoResponse` (axum) nor `Error` (mda-core) is defined in this crate.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;

pub use mda_core::Error;

/// The API-layer error wrapper.
pub struct ApiError(pub Error);

impl From<Error> for ApiError {
    fn from(e: Error) -> Self {
        Self(e)
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

/// Convenience alias for handler return types.
pub type ApiResult<T> = Result<T, ApiError>;

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (code, kind) = match &self.0 {
            Error::Invalid(_) => (StatusCode::UNPROCESSABLE_ENTITY, "invalid"),
            Error::NotFound(_) => (StatusCode::NOT_FOUND, "not_found"),
            Error::Conflict(_) => (StatusCode::CONFLICT, "conflict"),
            Error::Forbidden(_) => (StatusCode::FORBIDDEN, "forbidden"),
            Error::RateLimited(_) => (StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
            Error::Config(_) => (StatusCode::INTERNAL_SERVER_ERROR, "config_error"),
            Error::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
        };
        if code.is_server_error() {
            tracing::error!(error = %self.0, "request failed");
        } else {
            tracing::debug!(error = %self.0, "request rejected");
        }
        (
            code,
            Json(serde_json::json!({
                // Stable, machine-readable failure class (the SDK/i18n contract).
                // `code` is the canonical key; `error` is retained for legacy
                // clients and mirrors the HTTP status bucket.
                "code": self.0.code(),
                "error": kind,
                "status": code.as_u16(),
                "message": self.0.to_string(),
            })),
        )
            .into_response()
    }
}
