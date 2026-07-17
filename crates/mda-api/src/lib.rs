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
pub mod blobs;
pub mod data;
pub mod edge;
pub mod error;
pub mod events;
pub mod extract;
pub mod notifications;
pub mod reports;
pub mod studio;

use mda_meta::MetadataCache;

/// Shared application state injected into every handler.
#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub cache: MetadataCache,
    pub jwt: JwtConfig,
    pub blobs: std::sync::Arc<dyn crate::blobs::BlobStore>,
    /// Fan-out for the SSE real-time channel (§5.10); fed by `events::spawn_listen`.
    pub events: tokio::sync::broadcast::Sender<crate::events::EventRow>,
    /// Login brute-force defence (per-account lockout + per-IP limit), shared
    /// across instances via `sys.sys_login_throttle` (§3).
    pub login_throttle: mda_security::LoginThrottle,
}

/// Build the application router.
pub fn router(state: AppState) -> Router {
    router_with(state, edge::EdgeConfig::from_env())
}

/// Build the router with an explicit edge config (used by tests / config-driven
/// wiring). Applies CORS, security headers, a global body-size limit, and the
/// request-id / access-log / metrics middleware around all routes.
pub fn router_with(state: AppState, cfg: edge::EdgeConfig) -> Router {
    let app = Router::new()
        .route("/health", get(health))
        .route("/api/auth/me", get(auth::me))
        .merge(edge::routes())
        .merge(auth::routes())
        .merge(studio::routes())
        .merge(data::routes())
        .merge(reports::routes())
        .merge(notifications::routes())
        .merge(blobs::routes())
        .merge(events::routes());

    // Layer order (last = outermost): body-limit → security headers → access
    // log/metrics → CORS. CORS is outermost so preflight is answered before
    // anything else; the access log sees the final status.
    let app = app.layer(axum::extract::DefaultBodyLimit::max(cfg.max_body_bytes));
    let app = edge::apply_security_headers(app);
    app.layer(axum::middleware::from_fn(edge::access_log))
        .layer(edge::cors_layer(&cfg))
        .with_state(state)
}

#[derive(Debug, Serialize)]
struct HealthReport {
    status: &'static str,
    database: &'static str,
    version: &'static str,
    audit_failures: u64,
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
        audit_failures: crate::data::audit_failure_count(),
    };

    let code = if db_ok {
        axum::http::StatusCode::OK
    } else {
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    };

    (code, axum::Json(report)).into_response()
}
