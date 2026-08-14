//! Integration architecture (PLAN §5.22 / Phase 9): the hub model. Inbound
//! materializes external data into the canonical biz entity, keyed by external id
//! (idempotent upsert, no duplicates). Outbound pushes a biz record to an external
//! system through a connector. Plus the webhook→inbound path (Slice 4 → 5).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use mda_api::AppState;
use mda_meta::MetadataCache;
use mda_security::jwt::JwtConfig;
use serde_json::{json, Value};
use sqlx::PgPool;
use std::sync::Arc;
use tokio::sync::Mutex;
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

async fn mock_receiver() -> (String, Arc<Mutex<Option<Value>>>) {
    let captured: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let cap = captured.clone();
    let app = axum::Router::new().route(
        "/upsert",
        axum::routing::post(move |body: axum::body::Bytes| {
            let cap = cap.clone();
            async move {
                let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                *cap.lock().await = Some(v);
                StatusCode::OK
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), captured)
}

#[tokio::test]
async fn inbound_upserts_by_external_key_no_duplicates() {
    let Some(ctx) = setup().await else {
        return;
    };
    publish(
        &ctx,
        customer_model(&format!("cust_{}", Uuid::new_v4().simple())),
    )
    .await;

    // create an inbound flow mapping external → biz.
    let (_, f) = call(
        &ctx.app,
        "POST",
        "/api/flows",
        &ctx.token,
        Some(
            json!({"name":"sync_in","direction":"inbound","entity":"Customer",
                   "mapping":{"name":"name","tier":"tier"},
                   "external_key_field":"external_id","system":"acme"})
            .to_string(),
        ),
    )
    .await;
    let flow_id = f["id"].as_str().unwrap().parse::<Uuid>().unwrap();

    // first delivery: create.
    let (st, v) = call(
        &ctx.app,
        "POST",
        &format!("/api/flows/{flow_id}/run"),
        &ctx.token,
        Some(json!({"payload":{"external_id":"A1","name":"Acme","tier":"Gold"}}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let rec1 = v["record_id"].as_str().unwrap().parse::<Uuid>().unwrap();

    // second delivery, same key, different tier: update (no duplicate).
    let (st, v) = call(
        &ctx.app,
        "POST",
        &format!("/api/flows/{flow_id}/run"),
        &ctx.token,
        Some(json!({"payload":{"external_id":"A1","name":"Acme","tier":"Silver"}}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let rec2 = v["record_id"].as_str().unwrap().parse::<Uuid>().unwrap();
    assert_eq!(rec1, rec2, "same external key → same record");

    // one Customer row, tier Silver.
    let (count, tier): (i64, String) =
        sqlx::query_as("SELECT count(*), (SELECT tier FROM biz.customer_* LIMIT 1)")
            .fetch_one(&ctx.pool)
            .await
            .map_err(|e| {
                // dynamic table name; fall back to a count via the entity table.
                eprintln!("join failed (dynamic table): {e}");
            })
            .unwrap_or((0, String::new()));
    let _ = (count, tier);
    // the table name is dynamic, so query it by listing via the API instead.
    let (_, list) = call(&ctx.app, "GET", "/api/data/Customer", &ctx.token, None).await;
    let rows = list["items"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "no duplicate records");
    assert_eq!(rows[0]["tier"], "Silver");
    assert_eq!(rows[0]["name"], "Acme");

    // external-id registry resolves the key → record.
    let (st, v) = call(
        &ctx.app,
        "GET",
        "/api/external-ids/Customer/A1?system=acme",
        &ctx.token,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        v["record_id"].as_str().unwrap().parse::<Uuid>().unwrap(),
        rec1
    );

    // a different external key → a new record (no cross-contamination).
    let (_, v) = call(
        &ctx.app,
        "POST",
        &format!("/api/flows/{flow_id}/run"),
        &ctx.token,
        Some(json!({"payload":{"external_id":"B2","name":"Globex","tier":"Bronze"}}).to_string()),
    )
    .await;
    let rec3 = v["record_id"].as_str().unwrap().parse::<Uuid>().unwrap();
    assert_ne!(rec3, rec1);
    let (_, list) = call(&ctx.app, "GET", "/api/data/Customer", &ctx.token, None).await;
    assert_eq!(list["items"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn outbound_pushes_mapped_record_to_external() {
    let Some(ctx) = setup().await else {
        return;
    };
    publish(
        &ctx,
        customer_model(&format!("cust_{}", Uuid::new_v4().simple())),
    )
    .await;

    let (base_url, captured) = mock_receiver().await;

    // connector + outbound flow.
    let (_, c) = call(
        &ctx.app,
        "POST",
        "/api/connectors",
        &ctx.token,
        Some(
            json!({"name":"acme","transport":"http","base_url":base_url,"auth":{"kind":"none"}})
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
            json!({"name":"sync_out","direction":"outbound","entity":"Customer",
                   "connector_id":connector_id,"endpoint_path":"/upsert",
                   "mapping":{"name":"name","tier":"tier"}})
            .to_string(),
        ),
    )
    .await;
    let flow_id = f["id"].as_str().unwrap().parse::<Uuid>().unwrap();

    let (st, _) = call(
        &ctx.app,
        "POST",
        &format!("/api/flows/{flow_id}/run"),
        &ctx.token,
        Some(json!({"record":{"name":"Acme","tier":"Gold"}}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    let got = captured
        .lock()
        .await
        .clone()
        .expect("external received the push");
    assert_eq!(got["name"], "Acme");
    assert_eq!(got["tier"], "Gold");
}

#[tokio::test]
async fn inbound_value_map_step_translates_codes() {
    let Some(ctx) = setup().await else {
        return;
    };
    publish(
        &ctx,
        customer_model(&format!("cust_{}", Uuid::new_v4().simple())),
    )
    .await;

    // a value map translating external status codes → internal tiers.
    let mut tx = ctx.pool.begin().await.unwrap();
    mda_security::set_tenant(&mut tx, ctx.tenant).await.unwrap();
    sqlx::query("INSERT INTO int.value_map (tenant_id, name, entries) VALUES ($1,'tier_map',$2)")
        .bind(ctx.tenant)
        .bind(json!({"GOLD":"Gold","SILVER":"Silver","BRONZE":"Bronze"}))
        .execute(&mut *tx)
        .await
        .unwrap();
    let (flow_id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO int.flow (tenant_id, name, direction, entity, mapping, external_key_field, system)
         VALUES ($1,'vm_in','inbound','Customer','{}','external_id','acme') RETURNING id",
    )
    .bind(ctx.tenant)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO int.flow_step (flow_id, seq, kind, config)
         VALUES ($1, 1, 'value_map', $2)",
    )
    .bind(flow_id)
    .bind(json!({"field":"tier","map":"tier_map"}))
    .execute(&mut *tx)
    .await
    .unwrap();
    // map the external raw_tier into biz tier, then translate via the step.
    sqlx::query("UPDATE int.flow SET mapping = $2 WHERE id = $1")
        .bind(flow_id)
        .bind(json!({"name":"name","tier":"raw_tier"}))
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let (_, v) = call(
        &ctx.app,
        "POST",
        &format!("/api/flows/{flow_id}/run"),
        &ctx.token,
        Some(
            json!({"payload":{"external_id":"C3","name":"Initech","raw_tier":"GOLD"}}).to_string(),
        ),
    )
    .await;
    assert!(v["record_id"].as_str().is_some());

    let (_, list) = call(&ctx.app, "GET", "/api/data/Customer", &ctx.token, None).await;
    let row = &list["items"].as_array().unwrap()[0];
    assert_eq!(row["tier"], "Gold", "value_map translated GOLD→Gold");
}

#[tokio::test]
async fn webhook_to_inbound_flow_materializes_via_drain() {
    let Some(ctx) = setup().await else {
        return;
    };
    publish(
        &ctx,
        customer_model(&format!("cust_{}", Uuid::new_v4().simple())),
    )
    .await;

    // register a webhook + an inbound flow bound to it.
    let mut tx = ctx.pool.begin().await.unwrap();
    mda_security::set_tenant(&mut tx, ctx.tenant).await.unwrap();
    sqlx::query("INSERT INTO sys_secret (tenant_id, name, kind, ref) VALUES ($1,'in_secret','opaque','inkey')")
        .bind(ctx.tenant)
        .execute(&mut *tx)
        .await
        .unwrap();
    let (webhook_id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO int.webhook (tenant_id, name, url, event_types, secret_ref)
         VALUES ($1,'in','http://x','{}','in_secret') RETURNING id",
    )
    .bind(ctx.tenant)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    let (flow_id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO int.flow (tenant_id, name, direction, entity, webhook_id, mapping, external_key_field, system)
         VALUES ($1,'wh_in','inbound','Customer',$2,$3,'external_id','acme') RETURNING id",
    )
    .bind(ctx.tenant)
    .bind(webhook_id)
    .bind(json!({"name":"name","tier":"tier"}))
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();

    // rebuild the app with a secret store that has the value (for inbound verify).
    let secrets: Arc<dyn mda_core::SecretStore> =
        Arc::new(mda_api::secrets::LocalSecretStore::from_map(
            [("inkey".to_string(), "inboundsecret".to_string())]
                .into_iter()
                .collect(),
        ));
    let app = mda_api::router(AppState {
        pool: ctx.pool.clone(),
        cache: MetadataCache::new(),
        jwt: JwtConfig::from_env(),
        blobs: Arc::new(mda_api::blobs::LocalBlobStore::from_env()),
        secrets,
        events: mda_api::events::channel(),
        login_throttle: mda_security::LoginThrottle::default(),
        gql: std::sync::Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
    });

    // POST a signed inbound event to the webhook receiver.
    let body = json!({"external_id":"D4","name":"Umbrella","tier":"Gold"}).to_string();
    let ts = chrono::Utc::now().timestamp();
    let sig = mda_api::webhooks::sign(b"inboundsecret", ts, &body);
    let req = Request::post(format!("/api/integrations/webhooks/{webhook_id}"))
        .header("x-mda-signature", &sig)
        .header("x-mda-event-id", "evt-99")
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    // start the drain; it will run the inbound flow from the enqueued row.
    mda_server::outbox::spawn_drain(ctx.pool.clone());
    for _ in 0..40 {
        let pending: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM sys_outbox WHERE tenant_id=$1 AND status='pending'",
        )
        .bind(ctx.tenant)
        .fetch_one(&ctx.pool)
        .await
        .unwrap();
        if pending == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }

    // the Customer was materialized.
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM int_external_id WHERE tenant_id=$1 AND entity='Customer' AND external_key='D4'")
        .bind(ctx.tenant)
        .fetch_one(&ctx.pool)
        .await
        .unwrap();
    assert_eq!(n, 1, "external id registered");
    let _ = flow_id;
}
