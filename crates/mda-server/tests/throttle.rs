//! Login throttling (PLAN §3): per-account progressive lockout + per-IP rate
//! limit, shared across instances via Postgres. Exercises the HTTP path (the
//! login endpoint refuses with 429 once the threshold is hit, and a correct
//! password is *not* verified while locked) and the throttle module's
//! window/lockout timing directly.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use mda_api::AppState;
use mda_meta::MetadataCache;
use mda_security::jwt::JwtConfig;
use mda_security::LoginThrottle;
use serde_json::Value;
use sqlx::postgres::PgPoolOptions;
use std::time::Duration;
use tower::ServiceExt;
use uuid::Uuid;

mod common;

const GOOD_PASSWORD: &str = "correct-horse-battery-staple";

struct Ctx {
    app: axum::Router,
    pool: sqlx::PgPool,
    tenant: Uuid,
    email: String,
}

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
    let hash = mda_security::hash_password(GOOD_PASSWORD).unwrap();
    let email = format!("u{}@test", Uuid::new_v4().simple());
    let user_id = common::seed_user(&pool, tenant, &email, "admin", &hash).await;
    common::seed_assignment(&pool, tenant, user_id, role_id).await;
    let jwt = JwtConfig::from_env();
    let blobs: std::sync::Arc<dyn mda_api::blobs::BlobStore> =
        std::sync::Arc::new(mda_api::blobs::LocalBlobStore::from_env());
    let secrets: std::sync::Arc<dyn mda_core::SecretStore> =
        std::sync::Arc::new(mda_api::secrets::LocalSecretStore::from_env());
    let app_pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&app_role_url(&db_url))
        .await
        .unwrap_or_else(|e| {
            eprintln!("could not connect as mda_app ({e}); using owner pool");
            pool.clone()
        });
    let app = mda_api::router(AppState {
        pool: app_pool.clone(),
        cache: MetadataCache::new(),
        jwt: jwt.clone(),
        blobs,

        secrets,
        events: mda_api::events::channel(),
        login_throttle: LoginThrottle::default(),
    });
    Some(Ctx {
        app,
        pool,
        tenant,
        email,
    })
}

/// POST /api/auth/login and return (status, parsed JSON body).
async fn login(
    app: &axum::Router,
    tenant: Uuid,
    email: &str,
    password: &str,
) -> (StatusCode, Value) {
    let body = serde_json::json!({
        "tenant": tenant.to_string(),
        "email": email,
        "password": password,
    })
    .to_string();
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let st = resp.status();
    let bytes = to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap_or_default();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (st, v)
}

#[tokio::test]
async fn http_login_locks_after_threshold() {
    let Some(ctx) = setup().await else {
        return;
    };
    // Default threshold is 5. The first 5 attempts all return the credential
    // error (the lock is set during the 5th record_failure); the 6th is refused
    // with 429 — even with the correct password, since it's not verified.
    for i in 1..=5 {
        let (st, _) = login(&ctx.app, ctx.tenant, &ctx.email, "wrong").await;
        assert_ne!(
            st,
            StatusCode::TOO_MANY_REQUESTS,
            "attempt {i} should not be locked yet"
        );
    }
    let (st, body) = login(&ctx.app, ctx.tenant, &ctx.email, GOOD_PASSWORD).await;
    assert_eq!(st, StatusCode::TOO_MANY_REQUESTS, "locked: body={body}");
}

#[tokio::test]
async fn http_success_resets_the_counter() {
    let Some(ctx) = setup().await else {
        return;
    };
    // 4 failures (under the threshold of 5) ...
    for _ in 0..4 {
        login(&ctx.app, ctx.tenant, &ctx.email, "wrong").await;
    }
    // ... a successful login resets the burst ...
    let (st, _) = login(&ctx.app, ctx.tenant, &ctx.email, GOOD_PASSWORD).await;
    assert_eq!(st, StatusCode::OK);
    // ... so 4 more failures must not lock (4 < 5 after the reset).
    for _ in 0..4 {
        let (st, _) = login(&ctx.app, ctx.tenant, &ctx.email, "wrong").await;
        assert_ne!(
            st,
            StatusCode::TOO_MANY_REQUESTS,
            "should not lock after reset"
        );
    }
}

#[tokio::test]
async fn module_window_reset_and_lockout_expiry() {
    let Some(ctx) = setup().await else {
        return;
    };
    // Tiny durations so the window/lockout timing is exercised in seconds, not
    // the 15-minute defaults. (Margins are 200ms over the window/lockout.)
    let t = LoginThrottle {
        max_fails: 2,
        window: Duration::from_secs(1),
        lockout: Duration::from_secs(1),
    };
    let key = format!("test:{}", Uuid::new_v4());

    // 2 failures within the window → locked.
    t.record_failure(&ctx.pool, &key).await.unwrap();
    t.record_failure(&ctx.pool, &key).await.unwrap();
    assert!(
        t.is_locked(&ctx.pool, &key).await.unwrap(),
        "locked after 2 fails within the window"
    );

    // After the lockout elapses → unlocked.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(
        !t.is_locked(&ctx.pool, &key).await.unwrap(),
        "unlocked once the lockout elapsed"
    );

    // The burst window has also elapsed, so a fresh failure resets to count=1
    // (not locked), rather than building on the old burst.
    t.record_failure(&ctx.pool, &key).await.unwrap();
    assert!(
        !t.is_locked(&ctx.pool, &key).await.unwrap(),
        "window reset the burst; a single fresh fail isn't a lock"
    );

    // One more within the window → locked again (burst = 2).
    t.record_failure(&ctx.pool, &key).await.unwrap();
    assert!(
        t.is_locked(&ctx.pool, &key).await.unwrap(),
        "locked after a fresh burst of 2"
    );

    // Success clears the key entirely.
    t.record_success(&ctx.pool, &key).await.unwrap();
    assert!(
        !t.is_locked(&ctx.pool, &key).await.unwrap(),
        "success cleared the key"
    );
}

#[tokio::test]
async fn http_unknown_account_is_also_throttled() {
    // Brute-forcing an email that doesn't exist must still hit the lockout, so
    // an attacker can't bypass by guessing at non-existent accounts.
    let Some(ctx) = setup().await else {
        return;
    };
    let bogus = format!("nope-{}@test", Uuid::new_v4().simple());
    for _ in 0..5 {
        login(&ctx.app, ctx.tenant, &bogus, "wrong").await;
    }
    let (st, _) = login(&ctx.app, ctx.tenant, &bogus, "wrong").await;
    assert_eq!(
        st,
        StatusCode::TOO_MANY_REQUESTS,
        "unknown account locks too"
    );
}
