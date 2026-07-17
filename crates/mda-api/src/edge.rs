//! Production edge concerns: CORS, security headers, request body limits,
//! request-id + structured access logging, Prometheus `/metrics`, and the
//! k8s liveness/readiness split (`/livez`, `/readyz`).
//!
//! These are pure edge concerns — they don't touch business logic, so they
//! compose around the existing router without behaviour change. All knobs are
//! environment-driven with safe defaults (permissive only in debug builds).

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::set_header::SetResponseHeaderLayer;
use uuid::Uuid;

use crate::AppState;

/// Edge (non-auth) routes: health split + metrics. Mounted outside auth.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/livez", get(livez))
        .route("/readyz", get(ready))
        .route("/metrics", get(metrics_text))
}

/// Edge configuration parsed from the environment.
#[derive(Clone)]
pub struct EdgeConfig {
    /// Comma-separated allowed CORS origins (e.g. `https://app.example.com`).
    /// Empty in release → same-origin only (no CORS headers); permissive in
    /// debug for local development.
    pub cors_origins: Vec<String>,
    /// Max request body in bytes (applies to every route; covers JSON + uploads).
    pub max_body_bytes: usize,
}

impl Default for EdgeConfig {
    fn default() -> Self {
        Self::from_env()
    }
}

impl EdgeConfig {
    pub fn from_env() -> Self {
        let cors_origins = std::env::var("MDA_CORS_ORIGINS")
            .ok()
            .map(|s| {
                s.split(',')
                    .map(|o| o.trim().to_string())
                    .filter(|o| !o.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let max_body_bytes = std::env::var("MDA_MAX_BODY_BYTES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(10 * 1024 * 1024);
        Self {
            cors_origins,
            max_body_bytes,
        }
    }
}

/// Build the CORS layer. Explicit origins → credentials-safe allow-list. No
/// config → permissive in debug (dev convenience), same-origin in release
/// (`CorsLayer::permissive()` is a real cross-origin hole with credentials and
/// must never be the release default).
pub fn cors_layer(cfg: &EdgeConfig) -> CorsLayer {
    if cfg.cors_origins.is_empty() {
        return if cfg!(debug_assertions) {
            CorsLayer::permissive()
        } else {
            CorsLayer::new() // no allow-* → browser blocks cross-origin
        };
    }
    // Map to HeaderValues; invalid entries are dropped with a warning.
    let origins: Vec<HeaderValue> = cfg
        .cors_origins
        .iter()
        .filter_map(|o| o.parse::<HeaderValue>().ok())
        .collect();
    if origins.is_empty() {
        tracing::warn!("MDA_CORS_ORIGINS set but no value parsed as a valid origin");
        return CorsLayer::new();
    }
    CorsLayer::new()
        .allow_origin(AllowOrigin::list(origins))
        .allow_credentials(true)
        .allow_methods([
            axum::http::Method::GET,
            axum::http::Method::POST,
            axum::http::Method::PUT,
            axum::http::Method::PATCH,
            axum::http::Method::DELETE,
            axum::http::Method::OPTIONS,
        ])
        .allow_headers([
            axum::http::header::AUTHORIZATION,
            axum::http::header::CONTENT_TYPE,
            HeaderName::from_static("if-match"),
            HeaderName::from_static("last-event-id"),
            HeaderName::from_static("x-filename"),
            HeaderName::from_static("x-request-id"),
        ])
        .expose_headers([
            HeaderName::from_static("etag"),
            HeaderName::from_static("x-request-id"),
        ])
}

/// Security headers applied to every response (defense-in-depth; harmless when
/// the upstream LB/TLS terminator already sets them). Applied onto the router
/// so the concrete layer generic is never named in a collection.
pub fn apply_security_headers(app: Router<AppState>) -> Router<AppState> {
    const HEADERS: [(&str, &str); 4] = [
        ("x-content-type-options", "nosniff"),
        ("x-frame-options", "DENY"),
        ("referrer-policy", "no-referrer"),
        (
            "strict-transport-security",
            "max-age=31536000; includeSubDomains",
        ),
    ];
    let mut app = app;
    for (name, val) in HEADERS {
        app = app.layer(SetResponseHeaderLayer::if_not_present(
            HeaderName::from_static(name),
            HeaderValue::from_static(val),
        ));
    }
    app
}

/// Axum middleware: per-request id (honour inbound `x-request-id` or mint one),
/// structured access log, and a coarse request counter / latency sum for
/// `/metrics`. Infra paths (`/livez`, `/readyz`, `/metrics`) are not logged or
/// counted (k8s/scrape noise would drown out real traffic signals).
pub async fn access_log(req: Request, next: Next) -> Response {
    // Path-only by design: never log the query string. The SSE stream accepts
    // `?token=<jwt>` (browser EventSource can't set headers), so a full-URI log
    // here would capture bearer tokens. Preserve this if the log is ever changed.
    let path = req.uri().path().to_string();
    let is_infra = path == "/livez" || path == "/readyz" || path == "/metrics";

    let req_id = req
        .headers()
        .get("x-request-id")
        .cloned()
        .unwrap_or_else(|| HeaderValue::from_str(&Uuid::new_v4().to_string()).unwrap());

    let method = req.method().clone();
    let start = Instant::now();

    if !is_infra {
        metrics::in_flight_inc();
    }
    let mut resp = next.run(req).await;
    let dur = start.elapsed();
    if !is_infra {
        metrics::in_flight_dec();
        metrics::observe(dur);
    }

    if let Ok(id) = req_id.to_str().map(str::to_string) {
        let span = tracing::info_span!("request", %id);
        let _enter = span.enter();
        tracing::info!(
            method = %method,
            path = %path,
            status = %resp.status().as_u16(),
            dur_ms = dur.as_millis() as u64,
            "access"
        );
    }
    resp.headers_mut().insert("x-request-id", req_id);
    resp
}

// ===== /livez, /readyz, /metrics =====

/// `GET /livez` — liveness: the process is up and serving. Never depends on
/// the DB (k8s restarts the pod if this fails).
async fn livez() -> &'static str {
    "ok"
}

/// `GET /readyz` — readiness: the DB is reachable and the pod should receive
/// traffic. `GET /health` is kept as an alias for compatibility.
pub async fn ready(State(state): State<AppState>) -> Response {
    let db_ok = sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(&state.pool)
        .await
        .is_ok();
    let body = axum::Json(serde_json::json!({
        "status": if db_ok { "ok" } else { "degraded" },
        "database": if db_ok { "up" } else { "down" },
    }));
    let code = if db_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (code, body).into_response()
}

/// `GET /metrics` — Prometheus text exposition. No auth (scrape behind network
/// policy; like `/metrics` everywhere).
async fn metrics_text(State(state): State<AppState>) -> String {
    let (requests_total, in_flight, dur_us_sum) = metrics::snapshot();
    let audit_failures = crate::data::audit_failure_count();
    let pool_size = state.pool.size();
    let pool_idle = state.pool.num_idle();
    let mut s = String::new();
    s.push_str("# HELP mda_http_requests_total Total non-infra HTTP requests served.\n");
    s.push_str("# TYPE mda_http_requests_total counter\n");
    s.push_str(&format!("mda_http_requests_total {requests_total}\n\n"));
    s.push_str("# HELP mda_http_requests_in_flight Currently in-flight non-infra requests.\n");
    s.push_str("# TYPE mda_http_requests_in_flight gauge\n");
    s.push_str(&format!("mda_http_requests_in_flight {in_flight}\n\n"));
    s.push_str(
        "# HELP mda_http_request_duration_microseconds_sum Sum of request durations (micros).\n",
    );
    s.push_str("# TYPE mda_http_request_duration_microseconds_sum counter\n");
    s.push_str(&format!(
        "mda_http_request_duration_microseconds_sum {dur_us_sum}\n\n"
    ));
    s.push_str("# HELP mda_audit_write_failures_total Failed compliance-audit writes.\n");
    s.push_str("# TYPE mda_audit_write_failures_total counter\n");
    s.push_str(&format!(
        "mda_audit_write_failures_total {audit_failures}\n\n"
    ));
    s.push_str("# HELP mda_db_pool_size Acquired + idle DB connections.\n");
    s.push_str("# TYPE mda_db_pool_size gauge\n");
    s.push_str(&format!("mda_db_pool_size {pool_size}\n\n"));
    s.push_str("# HELP mda_db_pool_idle Idle DB connections.\n");
    s.push_str("# TYPE mda_db_pool_idle gauge\n");
    s.push_str(&format!("mda_db_pool_idle {pool_idle}\n"));
    s
}

// ===== in-process metrics (atomics; lock-free) =====

mod metrics {
    use super::*;
    static REQUESTS_TOTAL: AtomicU64 = AtomicU64::new(0);
    static IN_FLIGHT: AtomicI64 = AtomicI64::new(0);
    static DURATION_US_SUM: AtomicU64 = AtomicU64::new(0);

    pub fn observe(dur: Duration) {
        REQUESTS_TOTAL.fetch_add(1, Ordering::Relaxed);
        // as_micros is u128; saturate to u64 — a single request won't overflow.
        let _ = DURATION_US_SUM.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
            v.checked_add(dur.as_micros().min(u128::from(u64::MAX)) as u64)
        });
    }

    pub fn in_flight_inc() {
        IN_FLIGHT.fetch_add(1, Ordering::Relaxed);
    }

    pub fn in_flight_dec() {
        IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn snapshot() -> (u64, i64, u64) {
        (
            REQUESTS_TOTAL.load(Ordering::Relaxed),
            IN_FLIGHT.load(Ordering::Relaxed),
            DURATION_US_SUM.load(Ordering::Relaxed),
        )
    }
}
