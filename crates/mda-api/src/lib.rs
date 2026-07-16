//! `mda-api` — the HTTP edge (Axum).
//! - Phase 0: `/health`
//! - Phase 1: Studio API (draft → validate → publish)
//! - Phase 2: runtime data API (`/api/data/:entity`)
//! - Phase 3: JWT auth + object/field/record security + audit

use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use mda_security::jwt::JwtConfig;
use serde::Serialize;
use sqlx::PgPool;

pub mod auth;
pub mod data;
pub mod error;
pub mod extract;
pub mod reports;
pub mod studio;

use mda_meta::MetadataCache;

/// Shared application state injected into every handler.
#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub cache: MetadataCache,
    pub jwt: JwtConfig,
}

/// Build the application router.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/api/auth/me", get(auth::me))
        .merge(auth::routes())
        .merge(studio::routes())
        .merge(data::routes())
        .merge(reports::routes())
        .with_state(state)
}

#[derive(Debug, Serialize)]
struct HealthReport {
    status: &'static str,
    database: &'static str,
    version: &'static str,
}

/// `GET /health` — liveness + database connectivity (no auth).
async fn health(axum::extract::State(state): axum::extract::State<AppState>) -> Response {
    let db_ok = sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(&state.pool)
        .await
        .is_ok();

    let report = HealthReport {
        status: if db_ok { "ok" } else { "degraded" },
        database: if db_ok { "up" } else { "down" },
        version: env!("CARGO_PKG_VERSION"),
    };

    let code = if db_ok {
        axum::http::StatusCode::OK
    } else {
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    };

    (code, axum::Json(report)).into_response()
}
