//! API versioning & deprecation (PLAN §9 deferral), end-to-end through the
//! router layer. Verifies negotiation (header + Accept media type), the
//! `MDA-API-Version` discovery stamp on every response, deprecation signalling
//! (`Deprecation`/`Sunset`/`Link`), and the 400 rejection for an unsupported
//! major.
//!
//! Built with an explicit `EdgeConfig` (simulating a future `v2` current / `v1`
//! deprecated / floor `v1` world) so the test is independent of process env and
//! parallel-safe.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use mda_api::edge::EdgeConfig;
use mda_api::versioning::VersioningConfig;
use mda_api::AppState;
use mda_meta::MetadataCache;
use mda_security::jwt::JwtConfig;
use serde_json::Value;
use sqlx::PgPool;
use std::collections::HashSet;
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

/// An EdgeConfig simulating a post-v2-cutover world: current=2, v1 deprecated
/// (still served), floor=1. Pure data — no env mutation, parallel-safe.
fn v2_world() -> EdgeConfig {
    let mut deprecated = HashSet::new();
    deprecated.insert(1);
    EdgeConfig {
        cors_origins: vec![],
        max_body_bytes: 10 * 1024 * 1024,
        versioning: VersioningConfig {
            current: 2,
            min_supported: 1,
            deprecated,
            sunset: "Sun, 31 Dec 2099 00:00:00 GMT".into(),
            deprecation_link: "https://mda.example.com/docs/api-versioning".into(),
        },
    }
}

/// An EdgeConfig where v1 has been fully retired: current=3, floor=2 (v1
/// unsupported → 400).
fn v3_retired_v1() -> EdgeConfig {
    let mut deprecated = HashSet::new();
    deprecated.insert(2);
    EdgeConfig {
        cors_origins: vec![],
        max_body_bytes: 10 * 1024 * 1024,
        versioning: VersioningConfig {
            current: 3,
            min_supported: 2,
            deprecated,
            sunset: "Sun, 31 Dec 2099 00:00:00 GMT".into(),
            deprecation_link: "https://mda.example.com/docs/api-versioning".into(),
        },
    }
}

async fn setup_with(cfg: EdgeConfig) -> Option<Ctx> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let (pool, _db_url) = common::spawn_db(&url).await;
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
    let secrets: std::sync::Arc<dyn mda_core::SecretStore> =
        std::sync::Arc::new(mda_api::secrets::LocalSecretStore::from_env());
    let state = AppState {
        pool: pool.clone(),
        cache: MetadataCache::new(),
        jwt: jwt.clone(),
        blobs,
        secrets,
        events: mda_api::events::channel(),
        login_throttle: mda_security::LoginThrottle::default(),
        gql: std::sync::Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
    };
    let app = mda_api::router_with(state, cfg);
    Some(Ctx {
        app,
        token,
        pool,
        tenant,
    })
}

async fn get(
    app: &axum::Router,
    uri: &str,
    token: &str,
    extra: &[(&str, &str)],
) -> (StatusCode, Value, Vec<(String, String)>) {
    let mut b = Request::builder().method("GET").uri(uri);
    b = b.header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"));
    for (k, v) in extra {
        b = b.header(*k, *v);
    }
    let resp = app
        .clone()
        .oneshot(b.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let headers: Vec<(String, String)> = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let val: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, val, headers)
}

fn hdr<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

#[tokio::test]
async fn discovery_header_stamped_on_every_response() {
    let Some(ctx) = setup_with(v2_world()).await else {
        return;
    };
    // An unpinned request is served the current major (2) and stamped.
    let (st, _, h) = get(&ctx.app, "/health", &ctx.token, &[]).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(hdr(&h, "mda-api-version"), Some("2"));

    // An auth-protected route is also stamped (the layer wraps all routes).
    let (st, _, h) = get(&ctx.app, "/api/auth/me", &ctx.token, &[]).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(hdr(&h, "mda-api-version"), Some("2"));
}

#[tokio::test]
async fn pinned_deprecated_major_emits_sunset_headers() {
    let Some(ctx) = setup_with(v2_world()).await else {
        return;
    };
    // Pin v1 (deprecated in the v2 world) via the X-API-Version header.
    let (st, _, h) = get(&ctx.app, "/health", &ctx.token, &[("x-api-version", "1")]).await;
    assert_eq!(st, StatusCode::OK, "deprecated major is still served");
    assert_eq!(hdr(&h, "mda-api-version"), Some("1"));
    assert_eq!(hdr(&h, "deprecation"), Some("true"));
    assert!(hdr(&h, "sunset").is_some(), "Sunset header present");
    let link = hdr(&h, "link").expect("Link header present");
    assert!(
        link.contains("rel=\"deprecation\""),
        "Link rel=deprecation: {link}"
    );
}

#[tokio::test]
async fn accept_vendor_media_type_negotiates() {
    let Some(ctx) = setup_with(v2_world()).await else {
        return;
    };
    // Accept: application/vnd.mda+json; version=1 → deprecated v1.
    let (st, _, h) = get(
        &ctx.app,
        "/health",
        &ctx.token,
        &[("accept", "application/vnd.mda+json; version=1")],
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(hdr(&h, "mda-api-version"), Some("1"));
    assert_eq!(hdr(&h, "deprecation"), Some("true"));
}

#[tokio::test]
async fn unsupported_major_below_floor_is_rejected() {
    let Some(ctx) = setup_with(v3_retired_v1()).await else {
        return;
    };
    // v1 is below the floor (2) → 400 with the stable code + Sunset/Link.
    let (st, body, h) = get(&ctx.app, "/health", &ctx.token, &[("x-api-version", "1")]).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "mda.unsupported_version", "{body}");
    assert_eq!(body["requested_version"], 1);
    assert_eq!(body["minimum_supported_version"], 2);
    assert_eq!(hdr(&h, "mda-api-version"), Some("1"));
    assert!(hdr(&h, "sunset").is_some());
}

#[tokio::test]
async fn current_major_served_without_deprecation() {
    let Some(ctx) = setup_with(v2_world()).await else {
        return;
    };
    // Explicitly pin the current major (2) → no deprecation headers.
    let (st, _, h) = get(&ctx.app, "/health", &ctx.token, &[("x-api-version", "2")]).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(hdr(&h, "mda-api-version"), Some("2"));
    assert_eq!(
        hdr(&h, "deprecation"),
        None,
        "current major is not deprecated"
    );
}
