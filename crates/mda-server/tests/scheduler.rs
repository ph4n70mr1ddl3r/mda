//! Scheduled-job management (PLAN §14): cron-driven schedules with next-run /
//! last-run / failure state + per-run history. Covers the management API, the
//! worker firing due jobs, and the `report`/`custom` dispatch kinds.

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
    user_id: Uuid,
}

fn customer_model(table: &str) -> Value {
    json!({
        "modules": [],
        "entities": [{
            "id": Uuid::new_v4(), "module_id": null, "name": "Customer",
            "table_name": table, "label": "Customer", "description": null,
            "fields": [
                {"id": Uuid::new_v4(), "name":"name","label":"Name","field_type":"string","required":true,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{}},
                {"id": Uuid::new_v4(), "name":"tier","label":"Tier","field_type":"enum","required":false,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{"options":["Bronze","Silver","Gold"]}}
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
        user_id,
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

/// Author a saved report (count grouped by tier) and return its id.
async fn make_report(ctx: &Ctx) -> Uuid {
    let dataset = json!({
        "base_entity":"Customer",
        "fields":[{"field":"tier"},{"field":"*","aggregate":"count","alias":"n"}],
        "group_by":["tier"],
        "order_by":[{"field":"tier","asc":true}],
        "limit":10
    });
    let mut tx = ctx.pool.begin().await.unwrap();
    mda_security::set_tenant(&mut tx, ctx.tenant).await.unwrap();
    let (id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO meta.md_report (tenant_id, name, dataset) VALUES ($1,'by_tier',$2) RETURNING id",
    )
    .bind(ctx.tenant)
    .bind(&dataset)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    id
}

#[tokio::test]
async fn schedule_crud_and_management_api() {
    let Some(ctx) = setup().await else {
        return;
    };
    let target = Uuid::new_v4(); // arbitrary for a `custom` schedule

    // create
    let (st, v) = call(
        &ctx.app,
        "POST",
        "/api/schedules",
        &ctx.token,
        Some(
            json!({"name":"tick","kind":"custom","target_id":target,"cron":"* * * * * *"})
                .to_string(),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "create: {v}");
    assert_eq!(v["kind"], "custom");
    assert_eq!(v["enabled"], true);
    assert!(v["next_run"].is_string(), "armed on create: {v}");
    assert_eq!(v["running_user_id"], ctx.user_id.to_string()); // defaulted to creator
    let id = v["id"].as_str().unwrap().parse::<Uuid>().unwrap();

    // get
    let (st, v) = call(
        &ctx.app,
        "GET",
        &format!("/api/schedules/{id}"),
        &ctx.token,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["name"], "tick");

    // list (filtered by kind)
    let (st, list) = call(
        &ctx.app,
        "GET",
        "/api/schedules?kind=custom",
        &ctx.token,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 1);

    // a second, different kind is excluded by the filter
    let _ = call(
        &ctx.app,
        "POST",
        "/api/schedules",
        &ctx.token,
        Some(
            json!({"name":"rpt","kind":"report","target_id":target,"cron":"0 0 * * * *"})
                .to_string(),
        ),
    )
    .await;
    let (st, list) = call(&ctx.app, "GET", "/api/schedules", &ctx.token, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 2);

    // update: disable (disarms) + rename
    let (st, v) = call(
        &ctx.app,
        "PATCH",
        &format!("/api/schedules/{id}"),
        &ctx.token,
        Some(json!({"name":"tick2","enabled":false}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "patch: {v}");
    assert_eq!(v["name"], "tick2");
    assert_eq!(v["enabled"], false);
    assert!(v["next_run"].is_null(), "disabled disarms: {v}");

    // invalid cron rejected
    let (st, v) = call(
        &ctx.app,
        "POST",
        "/api/schedules",
        &ctx.token,
        Some(
            json!({"name":"bad","kind":"custom","target_id":target,"cron":"not a cron"})
                .to_string(),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "bad cron: {v}");

    // unknown kind rejected
    let (st, v) = call(
        &ctx.app,
        "POST",
        "/api/schedules",
        &ctx.token,
        Some(
            json!({"name":"bad","kind":"bogus","target_id":target,"cron":"* * * * * *"})
                .to_string(),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "bad kind: {v}");

    // delete
    let (st, _) = call(
        &ctx.app,
        "DELETE",
        &format!("/api/schedules/{id}"),
        &ctx.token,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (st, _) = call(
        &ctx.app,
        "GET",
        &format!("/api/schedules/{id}"),
        &ctx.token,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn worker_fires_due_schedule_and_records_history() {
    let Some(ctx) = setup().await else {
        return;
    };
    let target = Uuid::new_v4();

    // arm a custom schedule firing every second
    let (_, v) = call(
        &ctx.app,
        "POST",
        "/api/schedules",
        &ctx.token,
        Some(
            json!({"name":"every_sec","kind":"custom","target_id":target,"cron":"* * * * * *"})
                .to_string(),
        ),
    )
    .await;
    let id = v["id"].as_str().unwrap().parse::<Uuid>().unwrap();

    // start the worker; it should fire within a couple of ticks
    mda_api::schedules::spawn_scheduler(ctx.pool.clone());

    let mut fired = false;
    for _ in 0..40 {
        let (st, v) = call(
            &ctx.app,
            "GET",
            &format!("/api/schedules/{id}"),
            &ctx.token,
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        if v["last_status"] == "ok" {
            fired = true;
            // next_run advanced past the original due time
            assert!(v["next_run"].is_string(), "re-armed: {v}");
            assert!(v["last_run"].is_string(), "last_run stamped: {v}");
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    assert!(fired, "worker should have fired the due schedule");

    // run history recorded
    let (st, runs) = call(
        &ctx.app,
        "GET",
        &format!("/api/schedules/{id}/runs"),
        &ctx.token,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let runs = runs.as_array().unwrap();
    assert!(!runs.is_empty(), "at least one run row");
    assert_eq!(runs[0]["status"], "ok");
}

#[tokio::test]
async fn manual_trigger_runs_report_and_counts_rows() {
    let Some(ctx) = setup().await else {
        return;
    };
    publish(
        &ctx,
        customer_model(&format!("cust_{}", Uuid::new_v4().simple())),
    )
    .await;

    // two customers so the report has rows
    let _ = call(
        &ctx.app,
        "POST",
        "/api/data/Customer",
        &ctx.token,
        Some(json!({"name":"Acme","tier":"Gold"}).to_string()),
    )
    .await;
    let _ = call(
        &ctx.app,
        "POST",
        "/api/data/Customer",
        &ctx.token,
        Some(json!({"name":"Globex","tier":"Gold"}).to_string()),
    )
    .await;

    let rep_id = make_report(&ctx).await;

    // schedule the report (every minute; we'll trigger manually)
    let (st, v) = call(
        &ctx.app,
        "POST",
        "/api/schedules",
        &ctx.token,
        Some(
            json!({"name":"nightly","kind":"report","target_id":rep_id,"cron":"0 0 * * * *"})
                .to_string(),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "create: {v}");
    let id = v["id"].as_str().unwrap().parse::<Uuid>().unwrap();

    // manual trigger runs the report under the creator
    let (st, v) = call(
        &ctx.app,
        "POST",
        &format!("/api/schedules/{id}/run"),
        &ctx.token,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "trigger: {v}");
    assert_eq!(v["status"], "ok", "report ran: {v}");
    assert!(v["rows"].as_i64().unwrap() >= 1, "counted rows: {v}");

    // a run row was recorded
    let (_, runs) = call(
        &ctx.app,
        "GET",
        &format!("/api/schedules/{id}/runs"),
        &ctx.token,
        None,
    )
    .await;
    let runs = runs.as_array().unwrap();
    assert_eq!(runs[0]["status"], "ok");
    assert!(runs[0]["rows_affected"].as_i64().unwrap() >= 1);
}

/// A mock external source serving a fixed JSON array at `/customers`.
async fn mock_source(records: Value) -> String {
    let app = axum::Router::new().route(
        "/customers",
        axum::routing::get(move || {
            let r = records.clone();
            async move { axum::Json(r) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn integration_schedule_pulls_flow_from_connector() {
    let Some(ctx) = setup().await else {
        return;
    };
    publish(
        &ctx,
        customer_model(&format!("cust_{}", Uuid::new_v4().simple())),
    )
    .await;

    let base = mock_source(json!([
        {"external_id":"A1","name":"Acme","tier":"Gold"},
        {"external_id":"B2","name":"Globex","tier":"Silver"}
    ]))
    .await;

    // connector + inbound pull flow bound to it.
    let (_, c) = call(
        &ctx.app,
        "POST",
        "/api/connectors",
        &ctx.token,
        Some(
            json!({"name":"src","transport":"http","base_url":base,"auth":{"kind":"none"}})
                .to_string(),
        ),
    )
    .await;
    let connector_id = c["id"].as_str().unwrap().parse::<Uuid>().unwrap();
    let (_, f) = call(
        &ctx.app,
        "POST",
        "/api/flows",
        &ctx.token,
        Some(
            json!({"name":"pull","direction":"inbound","entity":"Customer",
                   "connector_id":connector_id,"endpoint_path":"/customers",
                   "mapping":{"name":"name","tier":"tier"},
                   "external_key_field":"external_id","system":"acme"})
            .to_string(),
        ),
    )
    .await;
    let flow_id = f["id"].as_str().unwrap().parse::<Uuid>().unwrap();

    // schedule the flow (the `integration` kind pulls on cadence).
    let (st, v) = call(
        &ctx.app,
        "POST",
        "/api/schedules",
        &ctx.token,
        Some(
            json!({"name":"nightly_pull","kind":"integration","target_id":flow_id,"cron":"0 0 * * * *"})
                .to_string(),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "create schedule: {v}");
    let sched_id = v["id"].as_str().unwrap().parse::<Uuid>().unwrap();

    // manual trigger pulls from the connector and materializes.
    let (st, v) = call(
        &ctx.app,
        "POST",
        &format!("/api/schedules/{sched_id}/run"),
        &ctx.token,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "trigger: {v}");
    assert_eq!(v["status"], "ok", "pull ran: {v}");
    assert_eq!(v["rows"].as_i64().unwrap(), 2, "two records pulled: {v}");

    // the Customers were materialized into the canonical entity.
    let (_, list) = call(&ctx.app, "GET", "/api/data/Customer", &ctx.token, None).await;
    assert_eq!(list["items"].as_array().unwrap().len(), 2);
}

/// Regression for migrations/20260132000001: the scheduler tables must live in
/// `public` — the production app role `mda_app` (no `mda` in its search_path)
/// reads them unqualified. They originally landed in the `mda` schema through
/// the owner role's `"$user"` search_path, so every scheduler query failed
/// with "relation does not exist" when the app served as `mda_app`.
#[tokio::test]
async fn scheduler_tables_are_visible_to_the_app_role() {
    let url = std::env::var("DATABASE_URL").unwrap();
    let (pool, _db) = common::spawn_db(&url).await;
    let mut conn = pool.acquire().await.unwrap();
    let has_role: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_roles WHERE rolname = 'mda_app')")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    if !has_role {
        return; // restricted environments without role-creation rights
    }
    sqlx::query("SET ROLE mda_app")
        .execute(&mut *conn)
        .await
        .unwrap();
    let schedules: i64 = sqlx::query_scalar("SELECT count(*) FROM sys_schedule")
        .fetch_one(&mut *conn)
        .await
        .expect("sys_schedule must resolve for mda_app (public schema)");
    let runs: i64 = sqlx::query_scalar("SELECT count(*) FROM sys_schedule_run")
        .fetch_one(&mut *conn)
        .await
        .expect("sys_schedule_run must resolve for mda_app (public schema)");
    assert_eq!((schedules, runs), (0, 0));
}
