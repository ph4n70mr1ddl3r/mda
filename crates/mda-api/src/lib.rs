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
use std::collections::HashMap;

pub mod admin;
pub mod auth;
pub mod blobs;
pub mod data;
pub mod edge;
pub mod error;
pub mod events;
pub mod extract;
pub mod graphql;
pub mod history;
pub mod i18n;
pub mod integrations;
pub mod mail;
pub mod notifications;
pub mod observability;
pub mod reports;
pub mod rules;
pub mod schedules;
pub mod secrets;
pub mod studio;
pub mod templates;
pub mod tenants;
pub mod ui;
pub mod versioning;
pub mod webhooks;
pub mod workflows;

use mda_meta::MetadataCache;

/// Shared application state injected into every handler.
#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub cache: MetadataCache,
    pub jwt: JwtConfig,
    pub blobs: std::sync::Arc<dyn crate::blobs::BlobStore>,
    /// Secrets (§5.20): values resolved server-side only, never returned by any
    /// API. `sys_secret` holds the reference; this resolves the value.
    pub secrets: std::sync::Arc<dyn mda_core::SecretStore>,
    /// Fan-out for the SSE real-time channel (§5.10); fed by `events::spawn_listen`.
    pub events: tokio::sync::broadcast::Sender<crate::events::EventRow>,
    /// Login brute-force defence (per-account lockout + per-IP limit), shared
    /// across instances via `sys.sys_login_throttle` (§3).
    pub login_throttle: mda_security::LoginThrottle,
    /// GraphQL schema cache, keyed by `(tenant_id, active_version)` so a publish
    /// (version advance) rebuilds the schema (ADR-0010).
    pub gql: std::sync::Arc<
        tokio::sync::RwLock<HashMap<(uuid::Uuid, i64), async_graphql::dynamic::Schema>>,
    >,
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
        .merge(admin::routes())
        .merge(data::routes())
        .merge(reports::routes())
        .merge(templates::routes())
        .merge(notifications::routes())
        .merge(webhooks::routes())
        .merge(blobs::routes())
        .merge(secrets::routes())
        .merge(events::routes())
        .merge(graphql::routes())
        .merge(i18n::routes())
        .merge(history::routes())
        .merge(integrations::routes())
        .merge(observability::routes())
        .merge(schedules::routes())
        .merge(tenants::routes())
        .merge(ui::routes())
        .merge(rules::routes())
        .merge(workflows::routes());

    // Layer order (last = outermost): body-limit → security headers → access
    // log/metrics → CORS. CORS is outermost so preflight is answered before
    // anything else; the access log sees the final status.
    //
    // API versioning is applied innermost so its `MDA-API-Version` discovery
    // header + deprecation signalling (and the 400 for an unsupported major)
    // reach every route, including future versioned surfaces (§7).
    let app = app.layer(axum::middleware::from_fn_with_state(
        cfg.versioning.clone(),
        crate::versioning::middleware,
    ));
    // A panicking handler becomes a 500 in the platform's error envelope (the
    // panic is logged; the client never sees a dropped connection). Applied
    // just outside versioning so the response still carries discovery headers.
    let app = app.layer(tower_http::catch_panic::CatchPanicLayer::custom(
        panic_response,
    ));
    let app = app.layer(axum::extract::DefaultBodyLimit::max(cfg.max_body_bytes));
    let app = edge::apply_security_headers(app);
    app.layer(axum::middleware::from_fn(edge::access_log))
        .layer(edge::cors_layer(&cfg))
        .with_state(state)
}

/// Convert a caught handler panic into the platform error envelope (500
/// `mda.internal_error`); the detail goes to the server log only.
fn panic_response(panic: Box<dyn std::any::Any + Send>) -> Response {
    let detail = panic
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".to_string());
    tracing::error!(detail = %detail, "handler panicked");
    crate::error::ApiError(mda_core::Error::Internal(anyhow::anyhow!(
        "handler panic: {detail}"
    )))
    .into_response()
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use tower::util::ServiceExt; // oneshot

    /// A panicking handler must surface as a 500 error envelope, never a
    /// dropped connection — this is what `CatchPanicLayer::custom(panic_response)`
    /// guarantees in the real router.
    #[tokio::test]
    async fn handler_panic_becomes_500_envelope() {
        async fn boom() -> &'static str {
            panic!("kaboom: secret detail")
        }
        let app = Router::new().route("/boom", get(boom)).layer(
            tower_http::catch_panic::CatchPanicLayer::custom(panic_response),
        );
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/boom")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .expect("response despite panic");
        assert_eq!(resp.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body");
        let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(body["code"], "mda.internal_error");
        assert_eq!(body["message"], "internal server error");
        assert!(!body["message"].as_str().unwrap().contains("secret detail"));
    }
}
