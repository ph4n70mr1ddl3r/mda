//! §14 "tenant observability console" — read-only, superuser-gated surfaces over
//! the operational tables (`sys_event_log`, `sys_outbox`, `md_migration_log`,
//! `sys_audit_log`). Verifies the four endpoints return tenant-scoped data and
//! that the superuser gate denies non-admins with the stable `mda.forbidden`
//! code.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use mda_api::AppState;
use mda_meta::MetadataCache;
use mda_security::jwt::JwtConfig;
use serde_json::{json, Value};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

mod common;

struct Ctx {
    app: axum::Router,
    token: String,
    jwt: JwtConfig,
    #[allow(dead_code)]
    pool: PgPool,
    tenant: Uuid,
}

async fn setup() -> Option<Ctx> {
    let url = match std::env::var("DATABASE_URL") {
        Ok(u) => u,
        Err(_) => {
            eprintln!("skipping: DATABASE_URL not set");
            return None;
        }
    };
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
    let app = mda_api::router(AppState {
        pool: pool.clone(),
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
        token,
        jwt,
        pool,
        tenant,
    })
}

async fn call(app: &axum::Router, method: &str, uri: &str, token: &str) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(uri);
    if token != "__none__" {
        b = b.header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let req = b.body(Body::empty()).unwrap();
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

async fn publish(ctx: &Ctx) {
    // Create a draft, put a Customer model, publish (writes md_migration_log for
    // the additive create-table op + an event_log row).
    let req = Request::builder()
        .method("POST")
        .uri("/api/studio/drafts")
        .header(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {}", ctx.token),
        )
        .header("content-type", "application/json")
        .body(Body::from(json!({"name":"p"}).to_string()))
        .unwrap();
    let resp = ctx.app.clone().oneshot(req).await.unwrap();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let d: Value = serde_json::from_slice(&bytes).unwrap();
    let id = d["id"].as_str().unwrap().parse::<Uuid>().unwrap();
    let etag = d["version_etag"].as_str().unwrap().to_string();

    let model = json!({
        "modules": [],
        "entities": [{
            "id": Uuid::new_v4(), "module_id": null, "name": "Customer",
            "table_name": format!("cust_{}", Uuid::new_v4().simple()),
            "label": "Customer", "description": null,
            "fields": [{"id": Uuid::new_v4(), "name":"name","label":"Name","field_type":"string","required":true,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{}}],
            "relationships": []
        }]
    });
    let req = Request::builder()
        .method("PUT")
        .uri(format!("/api/studio/drafts/{id}/model"))
        .header(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {}", ctx.token),
        )
        .header("if-match", etag)
        .header("content-type", "application/json")
        .body(Body::from(model.to_string()))
        .unwrap();
    let _ = ctx.app.clone().oneshot(req).await.unwrap();
    let _ = call(
        &ctx.app,
        "POST",
        &format!("/api/studio/drafts/{id}/publish"),
        &ctx.token,
    )
    .await;
}

#[tokio::test]
async fn observability_requires_admin() {
    let ctx = match setup().await {
        Some(c) => c,
        None => return,
    };
    // A non-superuser (read perm on a made-up entity) is denied on every console
    // endpoint with 403 + the stable code.
    let role_id = common::seed_role(&ctx.pool, ctx.tenant, "reader", &[("X", "read")]).await;
    let hash = mda_security::hash_password("x").unwrap();
    let email = format!("r{}@test", Uuid::new_v4().simple());
    let uid = common::seed_user(&ctx.pool, ctx.tenant, &email, "r", &hash).await;
    common::seed_assignment(&ctx.pool, ctx.tenant, uid, role_id).await;
    let token = ctx.jwt.issue_access(uid, ctx.tenant, None).unwrap();

    for path in [
        "/api/observability/events",
        "/api/observability/outbox",
        "/api/observability/migrations",
        "/api/observability/audit",
    ] {
        let (st, body) = call(&ctx.app, "GET", path, &token).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{path}: {body}");
        assert_eq!(body["code"], "mda.forbidden", "{path}: {body}");
    }
}

#[tokio::test]
async fn observability_read_capability_lets_modeler_see_redacted_trail() {
    // ADR-0018 follow-up: a non-admin principal granted `observability.read`
    // sees the console, and audit before/after is redacted (field-level
    // projection) while the rest of the trail (who/what/when) is visible.
    let ctx = match setup().await {
        Some(c) => c,
        None => return,
    };

    // seed an audit row directly (the console reads sys_audit_log).
    let mut tx = ctx.pool.begin().await.unwrap();
    mda_security::set_tenant(&mut tx, ctx.tenant).await.unwrap();
    sqlx::query(
        "INSERT INTO sys_audit_log (tenant_id, actor_id, entity, record_id, op, before, after)
         VALUES ($1, $2, 'Customer', $3, 'create', NULL, '{\"secret\":\"shh\"}'::jsonb)",
    )
    .bind(ctx.tenant)
    .bind(Uuid::new_v4())
    .bind(Uuid::new_v4())
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();

    // a modeler with the observability.read capability (no other perms).
    let role_id =
        common::seed_role(&ctx.pool, ctx.tenant, "ops", &[("*", "observability.read")]).await;
    let hash = mda_security::hash_password("x").unwrap();
    let email = format!("o{}@test", Uuid::new_v4().simple());
    let uid = common::seed_user(&ctx.pool, ctx.tenant, &email, "ops", &hash).await;
    common::seed_assignment(&ctx.pool, ctx.tenant, uid, role_id).await;
    let token = ctx.jwt.issue_access(uid, ctx.tenant, None).unwrap();

    // events surface is readable.
    let (st, _body) = call(&ctx.app, "GET", "/api/observability/events", &token).await;
    assert_eq!(st, StatusCode::OK);

    // audit trail is readable, but before/after are redacted for non-superusers.
    let (st, body) = call(&ctx.app, "GET", "/api/observability/audit", &token).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let item = &body["items"][0];
    assert_eq!(item["entity"], "Customer");
    assert!(item["after"].is_null(), "field payload redacted: {item}");

    // a principal without the capability (only a read perm) is still denied.
    let other = common::seed_role(&ctx.pool, ctx.tenant, "plain", &[("Customer", "read")]).await;
    let email2 = format!("p{}@test", Uuid::new_v4().simple());
    let uid2 = common::seed_user(&ctx.pool, ctx.tenant, &email2, "plain", &hash).await;
    common::seed_assignment(&ctx.pool, ctx.tenant, uid2, other).await;
    let token2 = ctx.jwt.issue_access(uid2, ctx.tenant, None).unwrap();
    let (st, body) = call(&ctx.app, "GET", "/api/observability/audit", &token2).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "mda.forbidden");
}

#[tokio::test]
async fn observability_surfaces_activity() {
    let ctx = match setup().await {
        Some(c) => c,
        None => return,
    };
    publish(&ctx).await;
    // create a Customer → audit + event rows.
    let req = Request::builder()
        .method("POST")
        .uri("/api/data/Customer")
        .header(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {}", ctx.token),
        )
        .header("content-type", "application/json")
        .body(Body::from(json!({"name":"Acme"}).to_string()))
        .unwrap();
    let _ = ctx.app.clone().oneshot(req).await.unwrap();

    // events: at least the record.created we just produced (tenant-scoped).
    let (st, body) = call(&ctx.app, "GET", "/api/observability/events", &ctx.token).await;
    assert_eq!(st, StatusCode::OK, "events: {body}");
    assert!(!body["items"].as_array().unwrap().is_empty());

    // audit: the create audit row.
    let (st, body) = call(&ctx.app, "GET", "/api/observability/audit", &ctx.token).await;
    assert_eq!(st, StatusCode::OK, "audit: {body}");
    assert!(body["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a["entity"] == "Customer" && a["op"] == "create"));

    // outbox: counts breakdown present (may be all-zero if no async side-effects
    // ran, but the shape must be a list).
    let (st, body) = call(&ctx.app, "GET", "/api/observability/outbox", &ctx.token).await;
    assert_eq!(st, StatusCode::OK, "outbox: {body}");
    assert!(body["counts"].is_array());
    assert!(body["outstanding"].is_array());

    // migrations: the publish wrote a log row.
    let (st, body) = call(&ctx.app, "GET", "/api/observability/migrations", &ctx.token).await;
    assert_eq!(st, StatusCode::OK, "migrations: {body}");
    assert!(!body["items"].as_array().unwrap().is_empty(), "{body}");
}

#[tokio::test]
async fn observability_filters_are_tenant_scoped() {
    // Cross-tenant isolation: tenant B's console must not see tenant A's events.
    let ctx = match setup().await {
        Some(c) => c,
        None => return,
    };
    publish(&ctx).await;
    let req = Request::builder()
        .method("POST")
        .uri("/api/data/Customer")
        .header(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {}", ctx.token),
        )
        .header("content-type", "application/json")
        .body(Body::from(json!({"name":"TenantA"}).to_string()))
        .unwrap();
    let _ = ctx.app.clone().oneshot(req).await.unwrap();

    // Tenant B admin in a different tenant.
    let tenant_b = Uuid::new_v4();
    let role_b = common::seed_role(&ctx.pool, tenant_b, "admin", &[("*", "*")]).await;
    let email_b = format!("b{}@test", Uuid::new_v4().simple());
    let user_b = common::seed_user(
        &ctx.pool,
        tenant_b,
        &email_b,
        "b",
        &mda_security::hash_password("x").unwrap(),
    )
    .await;
    common::seed_assignment(&ctx.pool, tenant_b, user_b, role_b).await;
    let token_b = ctx.jwt.issue_access(user_b, tenant_b, None).unwrap();

    let (st, body) = call(&ctx.app, "GET", "/api/observability/events", &token_b).await;
    assert_eq!(st, StatusCode::OK, "B events: {body}");
    // tenant B sees none of tenant A's Customer events.
    assert!(
        body["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["entity"] != "Customer"),
        "cross-tenant leak: {body}"
    );
}
