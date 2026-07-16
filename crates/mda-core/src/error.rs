//! Canonical error & result types for the MDA platform.

/// The canonical `Result` alias used across crates.
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("invalid input: {0}")]
    Invalid(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("forbidden: {0}")]
    Forbidden(String),

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
}
