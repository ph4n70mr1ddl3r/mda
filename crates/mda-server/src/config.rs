//! Environment-driven configuration.
//!
//! A Phase-0 stand-in for config-rs / figment (PLAN §3): explicit env vars with
//! sensible dev defaults. Swap in a file-aware loader later without changing
//! call sites.

use mda_core::{Error, Result};

#[derive(Debug, Clone)]
pub struct Settings {
    pub database_url: String,
    pub redis_url: String,
    pub host: String,
    pub port: u16,
    pub db_max_connections: u32,
    /// Optional non-superuser connection string (the `mda_app` role). The app
    /// serves requests through this pool so biz.* RLS engages. The owner
    /// `database_url` is still used for migrations + bootstrap.
    pub app_database_url: Option<String>,
    pub log_format: String,
    /// Fallback filter used only when `RUST_LOG` is unset.
    pub log_default: String,
}

impl Settings {
    pub fn load() -> Result<Self> {
        Ok(Self {
            database_url: env_req("DATABASE_URL")?,
            redis_url: env_or("REDIS_URL", "redis://127.0.0.1:6379"),
            host: env_or("MDA_HOST", "0.0.0.0"),
            port: parse_env("MDA_PORT", 8080)?,
            db_max_connections: parse_env("MDA_DB_MAX_CONNECTIONS", 10)?,
            app_database_url: std::env::var("MDA_APP_DATABASE_URL").ok(),
            log_format: env_or("LOG_FORMAT", "pretty"),
            log_default: "info,mda=debug,sqlx=warn".to_string(),
        })
    }
}

fn env_req(key: &str) -> Result<String> {
    std::env::var(key).map_err(|_| Error::Config(format!("{key} is required")))
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn parse_env<T: std::str::FromStr>(key: &str, default: T) -> Result<T> {
    match std::env::var(key) {
        Ok(v) => v
            .parse::<T>()
            .map_err(|_| Error::Config(format!("{key} is not a valid value: {v}"))),
        Err(_) => Ok(default),
    }
}
