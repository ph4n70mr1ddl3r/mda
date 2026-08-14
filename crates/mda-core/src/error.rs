//! Canonical error & result types for the MDA platform.
//!
//! Every platform error carries a **stable, machine-readable `code`** (the
//! `mda.<kind>` string below) in addition to its human `message`. The `code` is
//! the i18n key and the SDK contract: it never changes for a given failure
//! class, even if the English `message` wording is refined. This closes the
//! §14 "error code taxonomy + localized error messages" platform gap — clients
//! switch on `code`, humans read `message`, translators key on `code`.

use serde::Serialize;

/// The canonical `Result` alias used across crates.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A single field-level validation problem, surfaced in the `details` array of a
/// [`Error::Validation`] response. `code` is a stable machine key (the SDK/i18n
/// contract at field grain); `field` is the offending field path.
#[derive(Debug, Clone, Serialize)]
pub struct FieldError {
    pub field: String,
    pub code: &'static str,
    pub message: String,
}

impl FieldError {
    pub fn new(field: impl Into<String>, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            code,
            message: message.into(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("invalid input: {0}")]
    Invalid(String),

    /// One or more field-level validation problems. The envelope surfaces each
    /// problem in `details` so a client can render per-field errors in one round
    /// trip instead of fail-then-retry-per-field. `message` is the summary.
    #[error("validation failed: {message}")]
    Validation {
        message: String,
        fields: Vec<FieldError>,
    },

    #[error("not found: {0}")]
    NotFound(String),

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("forbidden: {0}")]
    Forbidden(String),

    #[error("too many requests: {0}")]
    RateLimited(String),

    #[error("internal error: {0}")]
    Internal(#[from] anyhow::Error),
}

impl Error {
    /// Wrap any error as an internal (500-class) error.
    pub fn internal<E>(e: E) -> Self
    where
        E: Into<anyhow::Error>,
    {
        Self::Internal(e.into())
    }

    /// The stable, machine-readable error code for this failure class.
    ///
    /// These strings are part of the public API contract: SDK clients branch on
    /// them and translators use them as message keys, so they must never change
    /// for a given variant (add new codes for new variants, don't rename).
    pub fn code(&self) -> &'static str {
        match self {
            Error::Config(_) => "mda.config_error",
            Error::Invalid(_) => "mda.invalid",
            Error::Validation { .. } => "mda.validation",
            Error::NotFound(_) => "mda.not_found",
            Error::Conflict(_) => "mda.conflict",
            Error::Forbidden(_) => "mda.forbidden",
            Error::RateLimited(_) => "mda.rate_limited",
            Error::Internal(_) => "mda.internal_error",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Error;

    /// The code set is the stable SDK/i18n contract — guard it against drift.
    #[test]
    fn codes_are_stable_and_unique() {
        let cases = [
            (Error::Config("x".into()), "mda.config_error"),
            (Error::Invalid("x".into()), "mda.invalid"),
            (
                Error::Validation {
                    message: "x".into(),
                    fields: vec![],
                },
                "mda.validation",
            ),
            (Error::NotFound("x".into()), "mda.not_found"),
            (Error::Conflict("x".into()), "mda.conflict"),
            (Error::Forbidden("x".into()), "mda.forbidden"),
            (Error::RateLimited("x".into()), "mda.rate_limited"),
            (Error::Internal(anyhow::anyhow!("x")), "mda.internal_error"),
        ];
        let mut seen = std::collections::HashSet::new();
        for (err, code) in cases {
            assert_eq!(err.code(), code);
            assert!(seen.insert(code), "duplicate code {code}");
            assert!(code.starts_with("mda."), "code must be namespaced: {code}");
        }
    }
}
