//! `mda-api` — the HTTP edge (Axum). Phase 0 added `/health`; Phase 1 adds the
//! Studio API (draft → validate → publish). Runtime, auth, GraphQL, and
//! real-time routes arrive in later phases.

use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde::Serialize;
use sqlx::PgPool;

pub mod data;
pub mod error;
pub mod extract;
pub mod studio;

use mda_meta::MetadataCache;

/// Shared application state injected into every handler.
#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub cache: MetadataCache,
}

/// Build the application router.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .merge(studio::routes())
        .merge(data::routes())
        .with_state(state)
}

#[derive(Debug, Serialize)]
struct HealthReport {
    status: &'static str,
    database: &'static str,
    version: &'static str,
}

/// `GET /health` — liveness + database connectivity.
///
/// Returns 200 `{ "status":"ok", "database":"up" }` when the DB answers
/// `SELECT 1`; 503 `database:"down"` otherwise. The platform is not "ready"
/// until the database is reachable.
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
