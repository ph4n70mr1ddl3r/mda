//! Session/refresh/logout lifecycle (PLAN §3): revocable refresh tokens with
//! rotation + reuse detection, logout, and token-type enforcement.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use mda_api::AppState;
use mda_meta::MetadataCache;
use mda_security::jwt::JwtConfig;
use serde_json::Value;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

mod common;

const PWD: &str = "correct-horse-battery-staple";

struct Ctx {
    app: axum::Router,
    tenant: Uuid,
    email: String,
    #[allow(dead_code)] // kept for tests that need direct (owner) DB access
    pool: PgPool,
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
    let hash = mda_security::hash_password(PWD).unwrap();
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
        login_throttle: mda_security::LoginThrottle::default(),
        gql: std::sync::Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
    });
    Some(Ctx {
        app,
        tenant,
        email,
        pool,
    })
}

/// POST a JSON body; return (status, parsed JSON body).
async fn post(app: &axum::Router, path: &str, body: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::post(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
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

async fn login(app: &axum::Router, tenant: Uuid, email: &str, pwd: &str) -> (StatusCode, Value) {
    post(
        app,
        "/api/auth/login",
        serde_json::json!({"tenant": tenant.to_string(), "email": email, "password": pwd}),
    )
    .await
}

async fn refresh(app: &axum::Router, token: &str) -> (StatusCode, Value) {
    post(
        app,
        "/api/auth/refresh",
        serde_json::json!({"refresh_token": token}),
    )
    .await
}

async fn logout(app: &axum::Router, access: &str) -> StatusCode {
    app.clone()
        .oneshot(
            Request::post("/api/auth/logout")
                .header("authorization", format!("Bearer {access}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

async fn me(app: &axum::Router, token: &str) -> StatusCode {
    app.clone()
        .oneshot(
            Request::get("/api/auth/me")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

fn access_of(v: &Value) -> String {
    v["access_token"].as_str().unwrap_or_default().to_string()
}
fn refresh_of(v: &Value) -> String {
    v["refresh_token"].as_str().unwrap_or_default().to_string()
}

#[tokio::test]
async fn login_issues_revocable_pair() {
    let Some(ctx) = setup().await else {
        return;
    };
    let (st, v) = login(&ctx.app, ctx.tenant, &ctx.email, PWD).await;
    assert_eq!(st, StatusCode::OK, "login: {v}");
    assert!(!access_of(&v).is_empty());
    assert!(!refresh_of(&v).is_empty());
    // The access token authenticates at /me.
    assert_eq!(me(&ctx.app, &access_of(&v)).await, StatusCode::OK);
}

#[tokio::test]
async fn refresh_rotates_and_reuse_revokes_all() {
    let Some(ctx) = setup().await else {
        return;
    };
    let (_, l) = login(&ctx.app, ctx.tenant, &ctx.email, PWD).await;
    let r1 = refresh_of(&l);

    // First refresh rotates the session → a fresh pair.
    let (st, v) = refresh(&ctx.app, &r1).await;
    assert_eq!(st, StatusCode::OK, "refresh: {v}");
    let r2 = refresh_of(&v);
    assert_ne!(r1, r2, "rotation must issue a new refresh token");

    // Reusing the old (now-rotated) refresh is rejected…
    let (st2, _) = refresh(&ctx.app, &r1).await;
    assert!(!st2.is_success(), "rotated refresh must be rejected");

    // …and reuse detection revoked EVERY session for the user, so the freshly
    // rotated refresh is dead too (refresh-token-theft containment).
    let (st3, _) = refresh(&ctx.app, &r2).await;
    assert!(!st3.is_success(), "reuse should have revoked all sessions");
}

#[tokio::test]
async fn logout_revokes_the_session() {
    let Some(ctx) = setup().await else {
        return;
    };
    let (_, l) = login(&ctx.app, ctx.tenant, &ctx.email, PWD).await;
    assert_eq!(
        logout(&ctx.app, &access_of(&l)).await,
        StatusCode::NO_CONTENT
    );
    // The session is revoked → its refresh token can no longer rotate.
    let (st, _) = refresh(&ctx.app, &refresh_of(&l)).await;
    assert!(!st.is_success(), "refresh after logout must fail");
}

#[tokio::test]
async fn token_types_cannot_be_swapped() {
    let Some(ctx) = setup().await else {
        return;
    };
    let (_, l) = login(&ctx.app, ctx.tenant, &ctx.email, PWD).await;
    let access = access_of(&l);
    let refresh_tok = refresh_of(&l);

    // Access authenticates; refresh must NOT be accepted as a bearer access token.
    assert_eq!(me(&ctx.app, &access).await, StatusCode::OK);
    assert_eq!(me(&ctx.app, &refresh_tok).await, StatusCode::UNAUTHORIZED);

    // And an access token must NOT be usable as a refresh token.
    let (st, _) = refresh(&ctx.app, &access).await;
    assert!(!st.is_success(), "access token must not refresh");
}

/// Login by tenant **slug** must work through the non-superuser app role:
/// `sec_tenant` is deliberately RLS-free (the public slug registry login
/// resolves pre-auth), but nothing else pinned that the app role can actually
/// read it — a future migration gating the table (or dropping its grant)
/// would break every slug login while UUID-login and token-minting tests
/// stayed green (the pass-1 works-as-owner class).
#[tokio::test]
async fn login_by_slug_works_as_the_app_role() {
    let Some(ctx) = setup().await else {
        return;
    };
    // Give this test's tenant a slug (owner pool; sec_tenant carries no RLS,
    // so no tenant GUC is needed — exactly why login can resolve it pre-auth).
    let slug = format!("t_{}", Uuid::new_v4().simple());
    sqlx::query("INSERT INTO sec.sec_tenant (id, slug, name) VALUES ($1,$2,$3)")
        .bind(ctx.tenant)
        .bind(&slug)
        .bind("Slug Tenant")
        .execute(&ctx.pool)
        .await
        .unwrap();

    // slug login (the app router serves through the mda_app pool)
    let (st, body) = post(
        &ctx.app,
        "/api/auth/login",
        serde_json::json!({"tenant": slug, "email": ctx.email, "password": PWD}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "slug login: {body}");
    assert!(body["access_token"].is_string(), "{body}");
    // the JWT carries the slug's tenant and authenticates
    assert_eq!(me(&ctx.app, &access_of(&body)).await, StatusCode::OK);

    // unknown slug fails closed with the same "invalid credentials" shape a
    // bad password produces (no tenant enumeration by status or message).
    let (st, body) = post(
        &ctx.app,
        "/api/auth/login",
        serde_json::json!({"tenant": "no-such-slug", "email": ctx.email, "password": PWD}),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    // contains, not ==: the API prefixes Display with "invalid input: "
    assert!(
        body["message"]
            .as_str()
            .unwrap_or("")
            .contains("invalid credentials"),
        "{body}"
    );
}
