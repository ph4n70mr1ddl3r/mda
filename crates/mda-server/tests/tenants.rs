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

#[tokio::test]
async fn tenant_import_round_trips_into_a_fresh_tenant() {
    // Export tenant A, then import the bundle into a *fresh* tenant B (which only
    // carries its bootstrap `admin` role). Every config row should reappear under
    // B by natural key, the A-side `admin` role merging into B's (same name),
    // the distinctive `Analyst` role + its permission seeding fresh, and the
    // model staged as a reviewable draft.
    let Some(a) = setup().await else {
        return;
    };
    publish(
        &a,
        customer_model(&format!("cust_{}", Uuid::new_v4().simple())),
    )
    .await;

    // distinctive config in A: an Analyst role + permission, an OWD row, a
    // report, a template, a notification type, a connector + flow, a schedule.
    let analyst_id = common::seed_role(&a.pool, a.tenant, "Analyst", &[("Customer", "read")]).await;
    let mut tx = a.pool.begin().await.unwrap();
    mda_security::set_tenant(&mut tx, a.tenant).await.unwrap();
    sqlx::query(
        "INSERT INTO sec.sec_owd (tenant_id, entity, default_access) VALUES ($1,'Customer','team')",
    )
    .bind(a.tenant)
    .execute(&mut *tx)
    .await
    .unwrap();
    let (report_id,): (Uuid,) = sqlx::query_as("INSERT INTO meta.md_report (tenant_id, name, dataset) VALUES ($1,'by_tier',$2) RETURNING id")
        .bind(a.tenant)
        .bind(json!({"base_entity":"Customer","fields":[{"field":"tier"}],"group_by":["tier"]}))
        .fetch_one(&mut *tx).await.unwrap();
    let (conn_id,): (Uuid,) = sqlx::query_as("INSERT INTO int.connector (tenant_id, name, base_url, auth) VALUES ($1,'acme','https://ext', $2) RETURNING id")
        .bind(a.tenant).bind(json!({"kind":"none"}))
        .fetch_one(&mut *tx).await.unwrap();
    let (_flow_id,): (Uuid,) = sqlx::query_as("INSERT INTO int.flow (tenant_id, name, direction, entity, connector_id, system) VALUES ($1,'sync_in','inbound','Customer',$2,'acme') RETURNING id")
        .bind(a.tenant).bind(conn_id)
        .fetch_one(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO meta.md_template (tenant_id, name, kind, body) VALUES ($1,'welcome','email','Hi {{name}}')")
        .bind(a.tenant).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO meta.md_notification_type (tenant_id, key, label, default_channels) VALUES ($1,'x.y','X Y','{in_app}')")
        .bind(a.tenant).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO meta.md_translation (tenant_id, locale, namespace, msg_key, value) VALUES ($1,'fr','ui','greeting','Bonjour')")
        .bind(a.tenant).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    // a report schedule (target_id → report) to exercise FK remapping.
    call(
        &a.app,
        "POST",
        "/api/schedules",
        &a.token,
        Some(
            json!({"name":"nightly","kind":"report","target_id": report_id,"cron":"0 0 * * * *"})
                .to_string(),
        ),
    )
    .await;

    // export A
    let (_, bundle) = call(&a.app, "GET", "/api/tenants/export", &a.token, None).await;
    assert!(!bundle["reports"].as_array().unwrap().is_empty());

    // fresh tenant B (only its bootstrap admin role + superuser)
    let Some(b) = setup().await else {
        return;
    };

    // B does not yet have any of A's distinctive config.
    let mut tx = b.pool.begin().await.unwrap();
    mda_security::set_tenant(&mut tx, b.tenant).await.unwrap();
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM meta.md_report")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(before, 0, "B starts with no reports");

    // import the bundle into B
    let (st, resp) = call(
        &b.app,
        "POST",
        "/api/tenants/import",
        &b.token,
        Some(bundle.to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "import ok: {resp}");
    assert_eq!(
        resp["restored"]["reports"], 1,
        "one report restored: {resp}"
    );
    assert_eq!(resp["restored"]["flows"], 1, "one flow restored: {resp}");
    assert_eq!(
        resp["restored"]["permissions"], 2,
        "admin + analyst perms restored: {resp}"
    );
    assert!(
        resp["draft_id"].as_str().is_some(),
        "model staged as a draft"
    );

    // verify B now carries the config by natural key
    let mut tx = b.pool.begin().await.unwrap();
    mda_security::set_tenant(&mut tx, b.tenant).await.unwrap();
    let report_n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM meta.md_report WHERE name='by_tier'")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    assert_eq!(report_n, 1, "report restored by name");
    let flow_n: i64 = sqlx::query_scalar("SELECT count(*) FROM int.flow WHERE name='sync_in'")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(flow_n, 1, "flow restored by name");
    // the flow's connector_id must still resolve (FK remap kept it valid).
    let dangling: i64 = sqlx::query_scalar("SELECT count(*) FROM int.flow f WHERE name='sync_in' AND connector_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM int.connector c WHERE c.id = f.connector_id)")
        .fetch_one(&mut *tx).await.unwrap();
    assert_eq!(dangling, 0, "flow connector FK intact after remap");
    let analyst_n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM sec.sec_role WHERE name='Analyst'")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    assert_eq!(analyst_n, 1, "Analyst role seeded");
    // the Analyst permission's role_id resolves to a real role (remap correct).
    let bad_perms: i64 = sqlx::query_scalar("SELECT count(*) FROM sec.sec_permission p WHERE NOT EXISTS (SELECT 1 FROM sec.sec_role r WHERE r.id = p.role_id)")
        .fetch_one(&mut *tx).await.unwrap();
    assert_eq!(
        bad_perms, 0,
        "all restored permissions reference a real role"
    );
    let owd_n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sec.sec_owd WHERE entity='Customer' AND default_access='team'",
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_eq!(owd_n, 1, "OWD restored");
    let tr_n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM meta.md_translation WHERE locale='fr' AND namespace='ui' AND msg_key='greeting' AND value='Bonjour'",
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_eq!(tr_n, 1, "translation restored by natural key");
    let sched_n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sys_schedule WHERE name='nightly' AND kind='report'",
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_eq!(
        sched_n, 1,
        "schedule restored; target remapped to B's report id"
    );
    let sched_target: Option<Uuid> =
        sqlx::query_scalar("SELECT target_id FROM sys_schedule WHERE name='nightly'")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    let report_in_b: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM meta.md_report WHERE name='by_tier'")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    assert_eq!(
        sched_target, report_in_b,
        "schedule target remapped to the report's id in B"
    );
    tx.commit().await.unwrap();

    // idempotent re-import: running it again changes no row counts (merge).
    let (_, bundle2) = call(&b.app, "GET", "/api/tenants/export", &b.token, None).await;
    let (st, resp2) = call(
        &b.app,
        "POST",
        "/api/tenants/import",
        &b.token,
        Some(bundle2.to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "re-import ok: {resp2}");
    let mut tx = b.pool.begin().await.unwrap();
    mda_security::set_tenant(&mut tx, b.tenant).await.unwrap();
    let report_n2: i64 =
        sqlx::query_scalar("SELECT count(*) FROM meta.md_report WHERE name='by_tier'")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    assert_eq!(report_n2, 1, "re-import does not duplicate");
    tx.commit().await.unwrap();

    // a non-admin cannot import.
    let reader = common::seed_role(&b.pool, b.tenant, "reader", &[("Customer", "read")]).await;
    let hash = mda_security::hash_password("x").unwrap();
    let email = format!("r{}@test", Uuid::new_v4().simple());
    let uid = common::seed_user(&b.pool, b.tenant, &email, "r", &hash).await;
    common::seed_assignment(&b.pool, b.tenant, uid, reader).await;
    let token = JwtConfig::from_env()
        .issue_access(uid, b.tenant, None)
        .unwrap();
    let (st, body) = call(
        &b.app,
        "POST",
        "/api/tenants/import",
        &token,
        Some(json!({"schema_version":1}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "mda.forbidden");

    let _ = analyst_id; // referenced for clarity
}

#[tokio::test]
async fn tenant_import_rejects_unsupported_schema() {
    let Some(ctx) = setup().await else {
        return;
    };
    let (st, body) = call(
        &ctx.app,
        "POST",
        "/api/tenants/import",
        &ctx.token,
        Some(json!({"schema_version": 99}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["code"], "mda.invalid");
}
