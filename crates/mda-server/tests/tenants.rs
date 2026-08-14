//! Tenant configuration export (PLAN §14 backup): a portable JSON snapshot of a
//! tenant's configuration — active model + reports + schedules + the security
//! graph + integration definitions.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use mda_api::AppState;
use mda_meta::MetadataCache;
use mda_security::jwt::JwtConfig;
use serde_json::{json, Value};
use sqlx::PgPool;
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;

mod common;

#[allow(dead_code)]
struct Ctx {
    app: axum::Router,
    token: String,
    pool: PgPool,
    tenant: Uuid,
}

fn customer_model(table: &str) -> Value {
    json!({
        "modules": [],
        "entities": [{
            "id": Uuid::new_v4(), "module_id": null, "name": "Customer",
            "table_name": table, "label": "Customer", "description": null,
            "fields": [
                {"id": Uuid::new_v4(), "name":"name","label":"Name","field_type":"string","required":true,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{}}
            ],
            "relationships": []
        }]
    })
}

async fn setup() -> Option<Ctx> {
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
    let blobs: Arc<dyn mda_api::blobs::BlobStore> =
        Arc::new(mda_api::blobs::LocalBlobStore::from_env());
    let secrets: Arc<dyn mda_core::SecretStore> =
        Arc::new(mda_api::secrets::LocalSecretStore::from_env());
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
        pool,
        tenant,
    })
}

async fn call(
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

async fn publish(ctx: &Ctx, model: Value) {
    let (_, d) = call(
        &ctx.app,
        "POST",
        "/api/studio/drafts",
        &ctx.token,
        Some(json!({"name":"p"}).to_string()),
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
        .body(Body::from(model.to_string()))
        .unwrap();
    let _ = ctx.app.clone().oneshot(req).await.unwrap();
    let (st, _) = call(
        &ctx.app,
        "POST",
        &format!("/api/studio/drafts/{id}/publish"),
        &ctx.token,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
}

#[tokio::test]
async fn tenant_export_snapshots_configuration() {
    let Some(ctx) = setup().await else {
        return;
    };
    publish(
        &ctx,
        customer_model(&format!("cust_{}", Uuid::new_v4().simple())),
    )
    .await;

    // a report + a schedule + a role (the security graph) to round out config.
    let mut tx = ctx.pool.begin().await.unwrap();
    mda_security::set_tenant(&mut tx, ctx.tenant).await.unwrap();
    sqlx::query("INSERT INTO meta.md_report (tenant_id, name, dataset) VALUES ($1,'by_tier',$2)")
        .bind(ctx.tenant)
        .bind(json!({"base_entity":"Customer","fields":[{"field":"tier"},{"field":"*","aggregate":"count","alias":"n"}],"group_by":["tier"]}))
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO sec.sec_owd (tenant_id, entity, default_access) VALUES ($1,'Customer','public_read')")
        .bind(ctx.tenant)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let (_, _) = call(
        &ctx.app,
        "POST",
        "/api/schedules",
        &ctx.token,
        Some(json!({"name":"nightly","kind":"custom","target_id": Uuid::new_v4(),"cron":"0 0 * * * *"}).to_string()),
    )
    .await;

    // export
    let (st, bundle) = call(&ctx.app, "GET", "/api/tenants/export", &ctx.token, None).await;
    assert_eq!(st, StatusCode::OK, "{bundle}");
    assert_eq!(bundle["schema_version"], 1);
    assert_eq!(bundle["tenant_id"], ctx.tenant.to_string());

    // the active model round-trips in the Studio shape.
    let entities = bundle["model"]["entities"].as_array().unwrap();
    assert!(entities.iter().any(|e| e["name"] == "Customer"));

    // reports, schedules, and the security graph are included.
    assert_eq!(bundle["reports"].as_array().unwrap().len(), 1);
    assert_eq!(bundle["reports"][0]["name"], "by_tier");
    assert_eq!(bundle["schedules"].as_array().unwrap().len(), 1);
    assert_eq!(bundle["schedules"][0]["name"], "nightly");
    assert_eq!(bundle["security"]["owd"].as_array().unwrap().len(), 1);
    // roles seeded by setup (admin) appear.
    assert!(
        bundle["security"]["roles"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["name"] == "admin"),
        "security graph exported"
    );

    // a non-admin is denied.
    let reader = common::seed_role(&ctx.pool, ctx.tenant, "reader", &[("Customer", "read")]).await;
    let hash = mda_security::hash_password("x").unwrap();
    let email = format!("r{}@test", Uuid::new_v4().simple());
    let uid = common::seed_user(&ctx.pool, ctx.tenant, &email, "r", &hash).await;
    common::seed_assignment(&ctx.pool, ctx.tenant, uid, reader).await;
    let token = JwtConfig::from_env()
        .issue_access(uid, ctx.tenant, None)
        .unwrap();
    let (st, body) = call(&ctx.app, "GET", "/api/tenants/export", &token, None).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "mda.forbidden");
}
