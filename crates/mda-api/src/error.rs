//! Map canonical [`mda_core::Error`] to HTTP responses.
//!
//! A newtype (`ApiError`) is required because of the orphan rules: neither
//! `IntoResponse` (axum) nor `Error` (mda-core) is defined in this crate.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::Value;

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
        let (code, kind, details) = match &self.0 {
            Error::Invalid(_) => (StatusCode::UNPROCESSABLE_ENTITY, "invalid", None),
            Error::Validation { fields, .. } => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "validation",
                // Surface every field problem at once (ADR-0018 refinement).
                Some(serde_json::to_value(fields).unwrap_or(Value::Null)),
            ),
            Error::NotFound(_) => (StatusCode::NOT_FOUND, "not_found", None),
            Error::Conflict(_) => (StatusCode::CONFLICT, "conflict", None),
            Error::Forbidden(_) => (StatusCode::FORBIDDEN, "forbidden", None),
            Error::RateLimited(_) => (StatusCode::TOO_MANY_REQUESTS, "rate_limited", None),
            Error::Config(_) => (StatusCode::INTERNAL_SERVER_ERROR, "config_error", None),
            Error::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal_error", None),
        };
        if code.is_server_error() {
            tracing::error!(error = %self.0, "request failed");
        } else {
            tracing::debug!(error = %self.0, "request rejected");
        }
        let mut body = serde_json::json!({
            // Stable, machine-readable failure class (the SDK/i18n contract).
            // `code` is the canonical key; `error` is retained for legacy
            // clients and mirrors the HTTP status bucket.
            "code": self.0.code(),
            "error": kind,
            "status": code.as_u16(),
            "message": self.0.to_string(),
        });
        if let Some(d) = details {
            body["details"] = d;
        }
        (code, Json(body)).into_response()
    }
}
