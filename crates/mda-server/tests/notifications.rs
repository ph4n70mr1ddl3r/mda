//! Notifications & messaging (PLAN §5.18): types, per-user preferences honored
//! at fan-out, multi-channel delivery (in-app + email), and digest roll-up.

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

#[allow(dead_code)]
struct Ctx {
    app: axum::Router,
    token: String,
    pool: PgPool,
    tenant: Uuid,
    user_id: Uuid,
    email: String,
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
        pool,
        tenant,
        user_id,
        email,
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

/// Wait for the drain to process the pending `notification.fanout` rows for the
/// tenant (delivers them into sys_notification / sys_message).
async fn wait_drained(pool: &PgPool, tenant: Uuid) {
    for _ in 0..30 {
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
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    panic!("outbox not drained");
}

#[tokio::test]
async fn notification_types_crud() {
    let Some(ctx) = setup().await else {
        return;
    };
    let (st, v) = call(
        &ctx.app,
        "POST",
        "/api/notification-types",
        &ctx.token,
        Some(
            json!({"key":"invoice.overdue","label":"Invoice Overdue",
                   "default_channels":["in_app","email"],"template_name":null})
            .to_string(),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    assert_eq!(v["key"], "invoice.overdue");
    assert_eq!(v["default_channels"], json!(["in_app", "email"]));

    // duplicate → conflict
    let (st, _) = call(
        &ctx.app,
        "POST",
        "/api/notification-types",
        &ctx.token,
        Some(json!({"key":"invoice.overdue","label":"x"}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);

    let (st, v) = call(
        &ctx.app,
        "GET",
        "/api/notification-types/invoice.overdue",
        &ctx.token,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["label"], "Invoice Overdue");
}

#[tokio::test]
async fn fanout_delivers_inapp_and_email_and_respects_preferences() {
    let Some(ctx) = setup().await else {
        return;
    };
    // a type with an email template body.
    call(
        &ctx.app,
        "POST",
        "/api/templates",
        &ctx.token,
        Some(json!({"name":"overdue","body":"Hi, {{ record.name }} is overdue"}).to_string()),
    )
    .await;
    call(
        &ctx.app,
        "POST",
        "/api/notification-types",
        &ctx.token,
        Some(
            json!({"key":"invoice.overdue","label":"Overdue",
                   "default_channels":["in_app","email"],"template_name":"overdue"})
            .to_string(),
        ),
    )
    .await;

    // the user opts OUT of email for this type.
    let (st, _) = call(
        &ctx.app,
        "PUT",
        "/api/notification-preferences",
        &ctx.token,
        Some(
            json!({"preferences":[{"type_key":"invoice.overdue","channel":"email","opted_in":false}]})
                .to_string(),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);

    // dispatch a notification to self.
    let (st, _) = call(
        &ctx.app,
        "POST",
        "/api/notifications/dispatch",
        &ctx.token,
        Some(
            json!({"type_key":"invoice.overdue","recipients":[ctx.user_id],
                   "context":{"record":{"name":"Acme"}}})
            .to_string(),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::ACCEPTED);

    mda_server::outbox::spawn_drain(ctx.pool.clone());
    wait_drained(&ctx.pool, ctx.tenant).await;

    // in-app delivered (email opted out → no sys_message row).
    let inapp: i64 =
        sqlx::query_scalar("SELECT count(*) FROM sys_notification WHERE tenant_id=$1 AND user_id=$2 AND type='invoice.overdue'")
            .bind(ctx.tenant)
            .bind(ctx.user_id)
            .fetch_one(&ctx.pool)
            .await
            .unwrap();
    assert_eq!(inapp, 1);

    let msgs: i64 =
        sqlx::query_scalar("SELECT count(*) FROM sys_message WHERE tenant_id=$1 AND user_id=$2")
            .bind(ctx.tenant)
            .bind(ctx.user_id)
            .fetch_one(&ctx.pool)
            .await
            .unwrap();
    assert_eq!(msgs, 0, "email was opted out → no message");
}

#[tokio::test]
async fn fanout_delivers_email_when_not_opted_out() {
    let Some(ctx) = setup().await else {
        return;
    };
    call(
        &ctx.app,
        "POST",
        "/api/templates",
        &ctx.token,
        Some(json!({"name":"welcome","body":"Welcome {{ record.name }}!"}).to_string()),
    )
    .await;
    call(
        &ctx.app,
        "POST",
        "/api/notification-types",
        &ctx.token,
        Some(
            json!({"key":"user.welcome","label":"Welcome",
                   "default_channels":["in_app","email"],"template_name":"welcome"})
            .to_string(),
        ),
    )
    .await;

    let (st, _) = call(
        &ctx.app,
        "POST",
        "/api/notifications/dispatch",
        &ctx.token,
        Some(
            json!({"type_key":"user.welcome","recipients":[ctx.user_id],
                   "context":{"record":{"name":"Ada"}}})
            .to_string(),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::ACCEPTED);

    mda_server::outbox::spawn_drain(ctx.pool.clone());
    wait_drained(&ctx.pool, ctx.tenant).await;

    // email rendered through the template + addressed to the user's email.
    let (to_addr, body): (String, String) = sqlx::query_as(
        "SELECT to_addr, body FROM sys_message WHERE tenant_id=$1 AND user_id=$2 LIMIT 1",
    )
    .bind(ctx.tenant)
    .bind(ctx.user_id)
    .fetch_one(&ctx.pool)
    .await
    .unwrap();
    assert_eq!(to_addr, ctx.email);
    assert_eq!(body, "Welcome Ada!");
}

#[tokio::test]
async fn digest_rolls_up_digestible_notifications() {
    let Some(ctx) = setup().await else {
        return;
    };
    // a digestible type; insert several unread notifications directly.
    sqlx::query(
        "INSERT INTO meta.md_notification_type (tenant_id, key, label, digestible)
         VALUES ($1, 'job.failed', 'Job Failed', TRUE)",
    )
    .bind(ctx.tenant)
    .execute(&ctx.pool)
    .await
    .unwrap();
    // Backdate them past the digest window (300s).
    for i in 0..3 {
        sqlx::query(
            "INSERT INTO sys_notification (tenant_id, user_id, type, payload, created_at)
             VALUES ($1, $2, 'job.failed', $3, now() - interval '600 seconds')",
        )
        .bind(ctx.tenant)
        .bind(ctx.user_id)
        .bind(json!({"i": i}))
        .execute(&ctx.pool)
        .await
        .unwrap();
    }
    // md_notification_type is RLS-gated; the insert above ran without the GUC →
    // would be blocked. Re-insert under the GUC if the count is zero.
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM meta.md_notification_type WHERE tenant_id=$1 AND key='job.failed'",
    )
    .bind(ctx.tenant)
    .fetch_one(&ctx.pool)
    .await
    .unwrap();
    if n == 0 {
        let mut tx = ctx.pool.begin().await.unwrap();
        mda_security::set_tenant(&mut tx, ctx.tenant).await.unwrap();
        sqlx::query(
            "INSERT INTO meta.md_notification_type (tenant_id, key, label, digestible)
             VALUES ($1, 'job.failed', 'Job Failed', TRUE)",
        )
        .bind(ctx.tenant)
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }

    let rolled = mda_api::notifications::digest_once(&ctx.pool)
        .await
        .unwrap();
    assert_eq!(rolled, 3);

    // originals marked digested; one summary notification created.
    let digested: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sys_notification WHERE tenant_id=$1 AND user_id=$2 AND digested_at IS NOT NULL",
    )
    .bind(ctx.tenant)
    .bind(ctx.user_id)
    .fetch_one(&ctx.pool)
    .await
    .unwrap();
    assert_eq!(digested, 3);
    let summary: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sys_notification WHERE tenant_id=$1 AND user_id=$2 AND type='job.failed.digest'",
    )
    .bind(ctx.tenant)
    .bind(ctx.user_id)
    .fetch_one(&ctx.pool)
    .await
    .unwrap();
    assert_eq!(summary, 1);
}

/// A Customer model with a `name` (readable) + `secret` (FLS-restrictable) field.
fn customer_model(table: &str) -> Value {
    json!({
        "modules": [],
        "entities": [{
            "id": Uuid::new_v4(), "module_id": null, "name": "Customer",
            "table_name": table, "label": "Customer", "description": null,
            "fields": [
                {"id": Uuid::new_v4(), "name":"name","label":"Name","field_type":"string","required":true,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{}},
                {"id": Uuid::new_v4(), "name":"secret","label":"Secret","field_type":"string","required":false,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{}}
            ],
            "relationships": []
        }]
    })
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

/// Seed a user with a given role (already created) and return its id.
async fn seed_user_with_role(pool: &PgPool, tenant: Uuid, role_id: Uuid) -> Uuid {
    let hash = mda_security::hash_password("x").unwrap();
    let email = format!("u{}@test", Uuid::new_v4().simple());
    let uid = common::seed_user(pool, tenant, &email, "n", &hash).await;
    common::seed_assignment(pool, tenant, uid, role_id).await;
    uid
}

#[tokio::test]
async fn record_readers_strategy_notifies_owner_and_share_not_others() {
    let Some(ctx) = setup().await else {
        return;
    };
    publish(
        &ctx,
        customer_model(&format!("cust_{}", Uuid::new_v4().simple())),
    )
    .await;

    // an in-app-only notification type.
    call(
        &ctx.app,
        "POST",
        "/api/notification-types",
        &ctx.token,
        Some(
            json!({"key":"cust.changed","label":"Changed","default_channels":["in_app"]})
                .to_string(),
        ),
    )
    .await;

    // a reader (read on Customer) + an outsider (no Customer access).
    let reader_role =
        common::seed_role(&ctx.pool, ctx.tenant, "reader", &[("Customer", "read")]).await;
    let reader = seed_user_with_role(&ctx.pool, ctx.tenant, reader_role).await;
    let outsider_role =
        common::seed_role(&ctx.pool, ctx.tenant, "outsider", &[("Invoice", "read")]).await;
    let outsider = seed_user_with_role(&ctx.pool, ctx.tenant, outsider_role).await;

    // a private Customer owned by the admin.
    let (_, c) = call(
        &ctx.app,
        "POST",
        "/api/data/Customer",
        &ctx.token,
        Some(json!({"name":"Acme","secret":"shh"}).to_string()),
    )
    .await;
    let rec_id = c["id"].as_str().unwrap().parse::<Uuid>().unwrap();

    // share the record with the reader (not the outsider).
    let mut tx = ctx.pool.begin().await.unwrap();
    mda_security::set_tenant(&mut tx, ctx.tenant).await.unwrap();
    sqlx::query(
        "INSERT INTO sec.sec_record_share (tenant_id, entity, record_id, principal_id, access)
         VALUES ($1, 'Customer', $2, $3, 'read')",
    )
    .bind(ctx.tenant)
    .bind(rec_id)
    .bind(reader)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();

    // dispatch to "everyone who can read this record".
    let (st, _) = call(
        &ctx.app,
        "POST",
        "/api/notifications/dispatch",
        &ctx.token,
        Some(
            json!({"type_key":"cust.changed","recipient_strategy":"record_readers",
                   "entity":"Customer","record_id":rec_id,"context":{"record":{"name":"Acme"}}})
            .to_string(),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::ACCEPTED);

    mda_server::outbox::spawn_drain(ctx.pool.clone());
    wait_drained(&ctx.pool, ctx.tenant).await;

    assert_eq!(count_notif(&ctx, ctx.user_id).await, 1, "owner notified");
    assert_eq!(
        count_notif(&ctx, reader).await,
        1,
        "share recipient notified"
    );
    assert_eq!(
        count_notif(&ctx, outsider).await,
        0,
        "outsider (no read access) NOT notified"
    );
}

/// Count a user's in-app notifications for the `cust.changed` type.
async fn count_notif(ctx: &Ctx, uid: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM sys_notification WHERE tenant_id=$1 AND user_id=$2 AND type='cust.changed'",
    )
    .bind(ctx.tenant)
    .bind(uid)
    .fetch_one(&ctx.pool)
    .await
    .unwrap()
}

/// Newest delivered message body for a user (for FLS-under-recipient checks).
async fn message_body(ctx: &Ctx, uid: Uuid) -> String {
    let (b,): (String,) = sqlx::query_as(
        "SELECT body FROM sys_message WHERE tenant_id=$1 AND user_id=$2 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(ctx.tenant)
    .bind(uid)
    .fetch_one(&ctx.pool)
    .await
    .unwrap();
    b
}

#[tokio::test]
async fn email_rendering_respects_recipient_fls() {
    let Some(ctx) = setup().await else {
        return;
    };
    publish(
        &ctx,
        customer_model(&format!("cust_{}", Uuid::new_v4().simple())),
    )
    .await;

    // a template that emits both the readable + the FLS-restricted field.
    call(
        &ctx.app,
        "POST",
        "/api/templates",
        &ctx.token,
        Some(
            json!({"name":"fls_test","body":"{{ record.name }} | {{ record.secret }}"}).to_string(),
        ),
    )
    .await;
    call(
        &ctx.app,
        "POST",
        "/api/notification-types",
        &ctx.token,
        Some(json!({"key":"cust.detail","label":"Detail","default_channels":["email"],"template_name":"fls_test"}).to_string()),
    )
    .await;

    // a reader with read on Customer but `secret` = none.
    let reader_role =
        common::seed_role(&ctx.pool, ctx.tenant, "reader2", &[("Customer", "read")]).await;
    let reader = seed_user_with_role(&ctx.pool, ctx.tenant, reader_role).await;
    let mut tx = ctx.pool.begin().await.unwrap();
    mda_security::set_tenant(&mut tx, ctx.tenant).await.unwrap();
    sqlx::query("INSERT INTO sec.sec_field_permission (role_id, entity, field, access) VALUES ($1,'Customer','secret','none')")
        .bind(reader_role)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    // a Customer owned by the admin, shared with the reader.
    let (_, c) = call(
        &ctx.app,
        "POST",
        "/api/data/Customer",
        &ctx.token,
        Some(json!({"name":"Acme","secret":"topsecret"}).to_string()),
    )
    .await;
    let rec_id = c["id"].as_str().unwrap().parse::<Uuid>().unwrap();
    let mut tx = ctx.pool.begin().await.unwrap();
    mda_security::set_tenant(&mut tx, ctx.tenant).await.unwrap();
    sqlx::query(
        "INSERT INTO sec.sec_record_share (tenant_id, entity, record_id, principal_id, access)
         VALUES ($1, 'Customer', $2, $3, 'read')",
    )
    .bind(ctx.tenant)
    .bind(rec_id)
    .bind(reader)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();

    // dispatch to everyone who can read the record, carrying both fields.
    let (st, _) = call(
        &ctx.app,
        "POST",
        "/api/notifications/dispatch",
        &ctx.token,
        Some(
            json!({"type_key":"cust.detail","recipient_strategy":"record_readers",
                   "entity":"Customer","record_id":rec_id,
                   "context":{"record":{"name":"Acme","secret":"topsecret"}}})
            .to_string(),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::ACCEPTED);
    mda_server::outbox::spawn_drain(ctx.pool.clone());
    wait_drained(&ctx.pool, ctx.tenant).await;

    // owner (full access) sees BOTH fields; reader (secret=none) sees only name.
    let owner_body = message_body(&ctx, ctx.user_id).await;
    let reader_body = message_body(&ctx, reader).await;
    assert!(
        owner_body.contains("Acme") && owner_body.contains("topsecret"),
        "owner sees both: {owner_body}"
    );
    assert!(
        reader_body.contains("Acme") && !reader_body.contains("topsecret"),
        "reader sees name only, secret FLS-projected: {reader_body}"
    );
}
