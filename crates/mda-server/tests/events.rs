//! `/api/events` SSE auth contract (PLAN §5.10).
//!
//! The stream authenticates from the `Authorization: Bearer <jwt>` header OR
//! `?token=<jwt>` (browser `EventSource` can't set headers). These tests lock
//! that auth behaviour + the `text/event-stream` response so the refactor that
//! pulled auth out of `AuthUser` can't silently regress. The handler's body is
//! an unbounded SSE stream, so we only assert status + headers here (not
//! consuming the body keeps the test bounded); the per-event AuthZ/payload
//! shape is covered by the unit-level `ChannelFilter`/`allow` logic.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use mda_api::AppState;
use mda_meta::MetadataCache;
use mda_security::jwt::JwtConfig;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

mod common;

struct Ctx {
    app: axum::Router,
    token: String,
    #[allow(dead_code)]
    pool: PgPool,
    #[allow(dead_code)]
    tenant: Uuid,
}

/// Build a DATABASE_URL that connects as the non-superuser `mda_app` role
/// (created by the RLS migration) by swapping the userinfo of `url`.
fn app_role_url(url: &str) -> String {
    if let Some(scheme_end) = url.find("://") {
        let rest = &url[scheme_end + 3..];
        if let Some(at) = rest.find('@') {
            return format!("{}://mda_app:mda@{}", &url[..scheme_end], &rest[at + 1..]);
        }
    }
    url.to_string()
}

async fn setup() -> Option<Ctx> {
    let url = match std::env::var("DATABASE_URL") {
        Ok(u) => u,
        Err(_) => {
            eprintln!("skipping: DATABASE_URL not set");
            return None;
        }
    };
    let (pool, db_url) = common::spawn_db(&url).await;
    let tenant = Uuid::new_v4();
    let role_id = common::seed_role(&pool, tenant, "admin", &[("*", "*")]).await;
    let hash = mda_security::hash_password("x").unwrap();
    let email = format!("u{}@test", Uuid::new_v4().simple());
    let user_id = common::seed_user(&pool, tenant, &email, "admin", &hash).await;
    common::seed_assignment(&pool, tenant, user_id, role_id).await;
    let jwt = JwtConfig::from_env();
    let token = jwt.issue_access(user_id, tenant, None).unwrap();
    let blobs: std::sync::Arc<dyn mda_api::blobs::BlobStore> =
        std::sync::Arc::new(mda_api::blobs::LocalBlobStore::from_env());
    let app_pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&app_role_url(&db_url))
        .await
        .unwrap_or_else(|e| {
            eprintln!(
                "RLS not exercised: could not connect as mda_app ({e}); falling back to owner pool"
            );
            pool.clone()
        });
    let app = mda_api::router(AppState {
        pool: app_pool.clone(),
        cache: MetadataCache::new(),
        jwt: jwt.clone(),
        blobs,
        events: mda_api::events::channel(),
        login_throttle: mda_security::LoginThrottle::default(),
    });
    Some(Ctx {
        app,
        token,
        pool,
        tenant,
    })
}

/// Send the request through the router and return the response status + the
/// `content-type` header value. Does NOT consume the (unbounded) SSE body.
async fn send(app: &axum::Router, req: Request<Body>) -> (StatusCode, Option<String>) {
    let resp = app.clone().oneshot(req).await.expect("router oneshot");
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    (resp.status(), ct)
}

#[tokio::test]
async fn sse_rejects_missing_and_invalid_token() {
    let Some(ctx) = setup().await else {
        return;
    };
    // No token at all (neither header nor query).
    let req = Request::get("/api/events").body(Body::empty()).unwrap();
    assert_eq!(send(&ctx.app, req).await.0, StatusCode::UNAUTHORIZED);

    // Bogus ticket via query.
    let req = Request::get("/api/events?ticket=not-a-ticket")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&ctx.app, req).await.0, StatusCode::UNAUTHORIZED);

    // Bogus token via header.
    let req = Request::get("/api/events")
        .header("authorization", "Bearer nope")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&ctx.app, req).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn sse_accepts_bearer_header_token() {
    let Some(ctx) = setup().await else {
        return;
    };
    let req = Request::get("/api/events")
        .header("authorization", format!("Bearer {}", ctx.token))
        .body(Body::empty())
        .unwrap();
    let (st, ct) = send(&ctx.app, req).await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        ct.as_deref()
            .unwrap_or_default()
            .starts_with("text/event-stream"),
        "content-type was {ct:?}"
    );
}

#[tokio::test]
async fn sse_accepts_short_lived_ticket() {
    // The browser `EventSource` path: it can't set headers, so the client first
    // POSTs /api/auth/event-ticket (with the access JWT) and opens the stream
    // with `?ticket=`. The ticket is one-shot + short-lived — NOT the access JWT
    // — so no long-lived credential lands in the URL/history/proxy-logs.
    let Some(ctx) = setup().await else {
        return;
    };
    let ticket = event_ticket(&ctx.app, &ctx.token).await;
    let url = format!("/api/events?ticket={ticket}");
    let req = Request::get(&url).body(Body::empty()).unwrap();
    let (st, ct) = send(&ctx.app, req).await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        ct.as_deref()
            .unwrap_or_default()
            .starts_with("text/event-stream"),
        "content-type was {ct:?}"
    );
}

/// POST /api/auth/event-ticket with `token` and return the issued ticket string.
async fn event_ticket(app: &axum::Router, token: &str) -> String {
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/auth/event-ticket")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "event-ticket issue failed");
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    v["ticket"].as_str().unwrap().to_string()
}
