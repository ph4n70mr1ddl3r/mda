//! Event & webhook contract (PLAN §5.21) + inbound verification (§14).
//!
//! Outbound: a webhook subscription → relay enqueues deliveries → drain signs +
//! POSTs the versioned envelope → the recipient sees a valid HMAC signature.
//! Inbound: a signed POST is verified (shared secret + replay window), deduped,
//! and recorded for an integration flow.

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
    token: Option<&str>,
    body: Option<String>,
    headers: Vec<(&str, &str)>,
) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        b = b.header(axum::http::header::AUTHORIZATION, format!("Bearer {t}"));
    }
    for (k, v) in &headers {
        b = b.header(*k, *v);
    }
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

/// A tiny mock webhook receiver: captures the envelope + signature.
async fn mock_receiver() -> (String, Arc<Mutex<Option<(String, String)>>>) {
    let captured: Arc<Mutex<Option<(String, String)>>> = Arc::new(Mutex::new(None));
    let cap = captured.clone();
    let app = axum::Router::new().route(
        "/hook",
        axum::routing::post(move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
            let cap = cap.clone();
            async move {
                let sig = headers
                    .get("x-mda-signature")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                let body = std::str::from_utf8(&body).unwrap_or("").to_string();
                *cap.lock().await = Some((sig, body));
                StatusCode::OK
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}/hook"), captured)
}

/// Wait until the drain has processed all pending rows for the tenant.
async fn wait_drained(pool: &PgPool, tenant: Uuid) {
    for _ in 0..40 {
        let pending: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM sys_outbox WHERE tenant_id=$1 AND status='pending'",
        )
        .bind(tenant)
        .fetch_one(pool)
        .await
        .unwrap();
        if pending == 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    panic!("outbox not drained");
}

#[tokio::test]
async fn webhook_subscriptions_crud() {
    let Some(ctx) = setup().await else {
        return;
    };
    // register the signing secret reference.
    let (st, _) = call(
        &ctx.app,
        "POST",
        "/api/secrets",
        Some(&ctx.token),
        Some(json!({"name":"wh_secret","ref":"whkey"}).to_string()),
        vec![],
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);

    let (st, v) = call(
        &ctx.app,
        "POST",
        "/api/webhooks",
        Some(&ctx.token),
        Some(
            json!({"name":"sync","url":"http://example.com/hook",
                   "event_types":["record.created"],"secret_ref":"wh_secret"}).to_string(),
        ),
        vec![],
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let id = v["id"].as_str().unwrap().parse::<Uuid>().unwrap();

    let (st, v) = call(
        &ctx.app,
        "GET",
        &format!("/api/webhooks/{id}"),
        Some(&ctx.token),
        None,
        vec![],
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["name"], "sync");

    let (st, _) = call(
        &ctx.app,
        "DELETE",
        &format!("/api/webhooks/{id}"),
        Some(&ctx.token),
        None,
        vec![],
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn outbound_delivery_signs_envelope() {
    let Some(ctx) = setup().await else {
        return;
    };
    let (hook_url, captured) = mock_receiver().await;

    // secret value provided via a LocalSecretStore built from a map.
    let secret_store: Arc<dyn mda_core::SecretStore> =
        Arc::new(mda_api::secrets::LocalSecretStore::from_map(
            [("whkey".to_string(), "topsecret".to_string())]
                .into_iter()
                .collect(),
        ));

    // register reference + subscription pointing at the mock receiver.
    let mut tx = ctx.pool.begin().await.unwrap();
    sqlx::query("INSERT INTO sys_secret (tenant_id, name, kind, ref) VALUES ($1,'wh_secret','opaque','whkey')")
        .bind(ctx.tenant)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let (st, v) = call(
        &ctx.app,
        "POST",
        "/api/webhooks",
        Some(&ctx.token),
        Some(
            json!({"name":"sync","url":hook_url,"event_types":["record.created"],
                   "secret_ref":"wh_secret"}).to_string(),
        ),
        vec![],
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let webhook_id = v["id"].as_str().unwrap().parse::<Uuid>().unwrap();

    // enqueue a webhook.deliver row directly.
    sqlx::query("INSERT INTO sys_outbox (tenant_id, kind, payload) VALUES ($1,'webhook.deliver',$2)")
        .bind(ctx.tenant)
        .bind(json!({
            "tenant_id": ctx.tenant,
            "webhook_id": webhook_id,
            "event_id": "42",
            "event_type": "record.created",
            "entity": "Customer",
            "record_id": Uuid::new_v4(),
            "data": {"changed_fields":["name"]}
        }))
        .execute(&ctx.pool)
        .await
        .unwrap();

    // drain with the secret store that actually has the value.
    mda_server::outbox::spawn_drain_with(
        ctx.pool.clone(),
        mda_api::notifications::default_channels(),
        secret_store,
        reqwest::Client::new(),
    );
    wait_drained(&ctx.pool, ctx.tenant).await;

    // the receiver got a signed envelope.
    let got = captured.lock().await.clone();
    let (sig, body) = got.expect("receiver was called");
    assert!(sig.starts_with("t=") && sig.contains("v1="));
    let env: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(env["schema_version"], 1);
    assert_eq!(env["event_id"], "42");
    assert_eq!(env["type"], "record.created");

    // the signature verifies against the secret + body.
    let ts: i64 = sig
        .split(',')
        .find(|p| p.starts_with("t="))
        .and_then(|p| p.strip_prefix("t="))
        .and_then(|s| s.parse().ok())
        .unwrap();
    let _ = ts;
    assert!(
        mda_api::webhooks::verify(b"topsecret", &sig, &body, chrono::Utc::now().timestamp())
            .is_ok()
    );

    // delivery recorded as delivered.
    let status: String = sqlx::query_scalar(
        "SELECT status FROM sys_webhook_delivery WHERE webhook_id=$1 AND event_id='42'",
    )
    .bind(webhook_id)
    .fetch_one(&ctx.pool)
    .await
    .unwrap();
    assert_eq!(status, "delivered");
}

#[tokio::test]
async fn relay_enqueues_deliveries_for_matching_events() {
    let Some(ctx) = setup().await else {
        return;
    };
    // subscription matching all events for entity "Customer".
    let mut tx = ctx.pool.begin().await.unwrap();
    mda_security::set_tenant(&mut tx, ctx.tenant).await.unwrap();
    sqlx::query("INSERT INTO sys_secret (tenant_id, name, kind, ref) VALUES ($1,'s','opaque','k')")
        .bind(ctx.tenant)
        .execute(&mut *tx)
        .await
        .unwrap();
    let (webhook_id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO int.webhook (tenant_id, name, url, event_types, entity_filter, secret_ref)
         VALUES ($1,'all','http://x/h', '{}', 'Customer', 's') RETURNING id",
    )
    .bind(ctx.tenant)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();

    // two events: one for Customer (matches), one for Invoice (does not).
    for entity in ["Customer", "Invoice"] {
        sqlx::query(
            "INSERT INTO sys_event_log (tenant_id, type, entity, record_id, payload)
             VALUES ($1, 'record.created', $2, $3, '{}'::jsonb)",
        )
        .bind(ctx.tenant)
        .bind(entity)
        .bind(Uuid::new_v4())
        .execute(&ctx.pool)
        .await
        .unwrap();
    }

    let enq = mda_api::webhooks::relay_once(&ctx.pool).await.unwrap();
    assert_eq!(enq, 1, "only the Customer event matches");

    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sys_outbox WHERE tenant_id=$1 AND kind='webhook.deliver'",
    )
    .bind(ctx.tenant)
    .fetch_one(&ctx.pool)
    .await
    .unwrap();
    assert_eq!(n, 1);

    // a second relay pass enqueues nothing (cursor advanced).
    let enq2 = mda_api::webhooks::relay_once(&ctx.pool).await.unwrap();
    assert_eq!(enq2, 0);
    let _ = webhook_id;
}

#[tokio::test]
async fn inbound_verifies_signature_and_dedupes() {
    let Some(ctx) = setup().await else {
        return;
    };
    let secret_store: Arc<dyn mda_core::SecretStore> =
        Arc::new(mda_api::secrets::LocalSecretStore::from_map(
            [("inkey".to_string(), "inboundsecret".to_string())]
                .into_iter()
                .collect(),
        ));
    // rebuild the app with the secret store that has the value.
    let app = mda_api::router(AppState {
        pool: ctx.pool.clone(),
        cache: MetadataCache::new(),
        jwt: JwtConfig::from_env(),
        blobs: Arc::new(mda_api::blobs::LocalBlobStore::from_env()),
        secrets: secret_store,
        events: mda_api::events::channel(),
        login_throttle: mda_security::LoginThrottle::default(),
    });

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
    tx.commit().await.unwrap();

    let body = json!({"type":"order.created","order":{"id":"A1","total":42}}).to_string();
    let ts = chrono::Utc::now().timestamp();
    let sig = mda_api::webhooks::sign(b"inboundsecret", ts, &body);

    // missing signature → forbidden
    let (st, _) = call(
        &app,
        "POST",
        &format!("/api/integrations/webhooks/{webhook_id}"),
        None,
        Some(body.clone()),
        vec![("x-mda-event-id", "evt-1")],
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);

    // valid signature + event id → accepted
    let (st, _) = call(
        &app,
        "POST",
        &format!("/api/integrations/webhooks/{webhook_id}"),
        None,
        Some(body.clone()),
        vec![
            ("x-mda-signature", sig.as_str()),
            ("x-mda-event-id", "evt-1"),
        ],
    )
    .await;
    assert_eq!(st, StatusCode::ACCEPTED);

    // duplicate event id → idempotent ack (OK, not re-recorded).
    let (st, _) = call(
        &app,
        "POST",
        &format!("/api/integrations/webhooks/{webhook_id}"),
        None,
        Some(body.clone()),
        vec![
            ("x-mda-signature", sig.as_str()),
            ("x-mda-event-id", "evt-1"),
        ],
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sys_inbound_webhook WHERE webhook_id=$1 AND event_id='evt-1'",
    )
    .bind(webhook_id)
    .fetch_one(&ctx.pool)
    .await
    .unwrap();
    assert_eq!(n, 1, "deduped on event_id");

    // a bad signature → forbidden
    let (st, _) = call(
        &app,
        "POST",
        &format!("/api/integrations/webhooks/{webhook_id}"),
        None,
        Some(body),
        vec![("x-mda-signature", "t=1,v1=deadbeef"), ("x-mda-event-id", "evt-2")],
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
}
