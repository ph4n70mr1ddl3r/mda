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
    /// Admin URL of the test database (owner role) — lets a test open a second
    /// pool as another role (e.g. `mda_app`) against the same database.
    db_url: String,
    tenant: Uuid,
    user_id: Uuid,
    email: String,
}

async fn setup() -> Option<Ctx> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let (pool, db_url) = common::spawn_db(&url).await;
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
        db_url,
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
    // A SECOND user with a single stale unread notification of the same
    // digestible type: a lone notification must never be rolled up
    // (`HAVING count(*) > 1`) — no `.digest` summary of one, and the
    // original stays in the timeline.
    let solo_email = format!("solo{}@test", Uuid::new_v4().simple());
    let solo = common::seed_user(
        &ctx.pool,
        ctx.tenant,
        &solo_email,
        "admin",
        &mda_security::hash_password("x").unwrap(),
    )
    .await;
    sqlx::query(
        "INSERT INTO sys_notification (tenant_id, user_id, type, payload, created_at)
         VALUES ($1, $2, 'job.failed', $3, now() - interval '600 seconds')",
    )
    .bind(ctx.tenant)
    .bind(solo)
    .bind(json!({"lone": true}))
    .execute(&ctx.pool)
    .await
    .unwrap();
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

    // the lone notification was left exactly as it was: still unread and
    // undigested, with no summary beside it.
    let lone_untouched: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sys_notification
          WHERE tenant_id=$1 AND user_id=$2 AND read_at IS NULL AND digested_at IS NULL",
    )
    .bind(ctx.tenant)
    .bind(solo)
    .fetch_one(&ctx.pool)
    .await
    .unwrap();
    assert_eq!(lone_untouched, 1, "a lone notification is never rolled up");
    let lone_summary: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sys_notification
          WHERE tenant_id=$1 AND user_id=$2 AND type='job.failed.digest'",
    )
    .bind(ctx.tenant)
    .bind(solo)
    .fetch_one(&ctx.pool)
    .await
    .unwrap();
    assert_eq!(lone_summary, 0, "no digest-of-one was created");
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

// ===== team hierarchy: ancestor-team recipient resolution (ADR-0013) =====
//
// A record owned in a sub-team should notify members of every ancestor
// (manager) team too (the `record_readers` strategy resolves the OWD-team
// reader set, which now includes ancestors). Mirrors the visibility predicate.

/// Seed a team (returns its id) under the tenant GUC.
async fn seed_team(pool: &PgPool, tenant: Uuid, name: &str) -> Uuid {
    let mut tx = pool.begin().await.unwrap();
    mda_security::set_tenant(&mut tx, tenant).await.unwrap();
    let (id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO sec.sec_team (tenant_id, name) VALUES ($1, $2)
         ON CONFLICT (tenant_id, name) DO UPDATE SET name = $2 RETURNING id",
    )
    .bind(tenant)
    .bind(name)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    id
}

/// Assign a user to a team under the tenant GUC.
async fn assign_team(pool: &PgPool, tenant: Uuid, user_id: Uuid, team_id: Uuid) {
    let mut tx = pool.begin().await.unwrap();
    mda_security::set_tenant(&mut tx, tenant).await.unwrap();
    sqlx::query("UPDATE sec.sec_user SET team_id = $1 WHERE id = $2")
        .bind(team_id)
        .bind(user_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

/// Set a team's parent (the hierarchy edge).
async fn set_team_parent(pool: &PgPool, tenant: Uuid, child: Uuid, parent: Uuid) {
    let mut tx = pool.begin().await.unwrap();
    mda_security::set_tenant(&mut tx, tenant).await.unwrap();
    sqlx::query("UPDATE sec.sec_team SET parent_id = $1 WHERE id = $2")
        .bind(parent)
        .bind(child)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

/// Set the OWD for an entity under the tenant GUC.
async fn seed_owd(pool: &PgPool, tenant: Uuid, entity: &str, access: &str) {
    let mut tx = pool.begin().await.unwrap();
    mda_security::set_tenant(&mut tx, tenant).await.unwrap();
    sqlx::query(
        "INSERT INTO sec.sec_owd (tenant_id, entity, default_access)
         VALUES ($1, $2, $3)
         ON CONFLICT (tenant_id, entity) DO UPDATE SET default_access = $3",
    )
    .bind(tenant)
    .bind(entity)
    .bind(access)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn record_readers_notifies_ancestor_team_members() {
    // Tree:  parent
    //          └─ child
    // A record owned in `child` should notify members of `child` AND `parent`
    // (ancestor). It must NOT notify a member of a sibling team.
    let Some(ctx) = setup().await else {
        return;
    };
    publish(
        &ctx,
        customer_model(&format!("cust_{}", Uuid::new_v4().simple())),
    )
    .await;
    seed_owd(&ctx.pool, ctx.tenant, "Customer", "team").await;
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

    let parent = seed_team(&ctx.pool, ctx.tenant, "parent").await;
    let child = seed_team(&ctx.pool, ctx.tenant, "child").await;
    let sibling = seed_team(&ctx.pool, ctx.tenant, "sibling").await;
    set_team_parent(&ctx.pool, ctx.tenant, child, parent).await;
    set_team_parent(&ctx.pool, ctx.tenant, sibling, parent).await;

    // owner in `child`; manager in `parent`; outsider in `sibling`. All carry
    // object-level read on Customer so the gate is cleared; the owner also
    // needs create.
    let owner_role = common::seed_role(
        &ctx.pool,
        ctx.tenant,
        "owner",
        &[("Customer", "create"), ("Customer", "read")],
    )
    .await;
    let reader_role =
        common::seed_role(&ctx.pool, ctx.tenant, "reader", &[("Customer", "read")]).await;
    let owner = seed_user_with_role(&ctx.pool, ctx.tenant, owner_role).await;
    let manager = seed_user_with_role(&ctx.pool, ctx.tenant, reader_role).await;
    let outsider = seed_user_with_role(&ctx.pool, ctx.tenant, reader_role).await;
    assign_team(&ctx.pool, ctx.tenant, owner, child).await;
    assign_team(&ctx.pool, ctx.tenant, manager, parent).await;
    assign_team(&ctx.pool, ctx.tenant, outsider, sibling).await;

    // owner (in child) creates the record.
    let owner_jwt = JwtConfig::from_env();
    let owner_token = owner_jwt.issue_access(owner, ctx.tenant, None).unwrap();
    let (_, c) = call(
        &ctx.app,
        "POST",
        "/api/data/Customer",
        &owner_token,
        Some(json!({"name":"Sub-team owned"}).to_string()),
    )
    .await;
    let rec_id = c["id"].as_str().unwrap().parse::<Uuid>().unwrap();

    // dispatch to everyone who can read the record.
    let (st, _) = call(
        &ctx.app,
        "POST",
        "/api/notifications/dispatch",
        &ctx.token,
        Some(
            json!({"type_key":"cust.changed","recipient_strategy":"record_readers",
                   "entity":"Customer","record_id":rec_id,
                   "context":{"record":{"name":"Sub-team owned"}}})
            .to_string(),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::ACCEPTED);
    mda_server::outbox::spawn_drain(ctx.pool.clone());
    wait_drained(&ctx.pool, ctx.tenant).await;

    assert_eq!(count_notif(&ctx, owner).await, 1, "owner notified");
    assert_eq!(
        count_notif(&ctx, manager).await,
        1,
        "ancestor-team member notified (manager visibility)"
    );
    assert_eq!(
        count_notif(&ctx, outsider).await,
        0,
        "sibling-team member NOT notified"
    );
}

/// Swap the userinfo of a postgres URL (owner → `mda_app`, whose password the
/// migration chain sets to `mda` when it creates the role).
fn as_app_role(db_url: &str) -> String {
    let (scheme, rest) = db_url.split_once("://").expect("url scheme");
    let (_, host) = rest.split_once('@').expect("url userinfo");
    format!("{scheme}://mda_app:mda@{host}")
}

/// The digest sweep must work under the NON-SUPERUSER app role. The
/// digestible-type join hits `meta.md_notification_type` (ENABLE+FORCE RLS):
/// the pre-fix sweep joined it on a tenant-less pool, saw zero rows as
/// `mda_app`, and silently never fired in any production deployment — while
/// staying green in tests, which run as the table owner (HARDENING pass 3's
/// `int.flow_step` bug class).
#[tokio::test]
async fn digest_sweep_works_as_the_app_role() {
    let Some(ctx) = setup().await else {
        return;
    };
    // restricted environments without role-creation rights: nothing to prove.
    let has_role: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_roles WHERE rolname = 'mda_app')")
            .fetch_one(&ctx.pool)
            .await
            .unwrap();
    if !has_role {
        return;
    }

    // seed (as the owner, under the GUC): one digestible type + 3 stale unread
    // notifications.
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
    for i in 0..3 {
        sqlx::query(
            "INSERT INTO sys_notification (tenant_id, user_id, type, payload, created_at)
             VALUES ($1, $2, 'job.failed', $3, now() - interval '600 seconds')",
        )
        .bind(ctx.tenant)
        .bind(ctx.user_id)
        .bind(json!({ "i": i }))
        .execute(&mut *tx)
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();

    // run the sweep connected AS mda_app (production configuration).
    let app_pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&as_app_role(&ctx.db_url))
        .await
        .unwrap();
    let rolled = mda_api::notifications::digest_once(&app_pool)
        .await
        .unwrap();
    assert_eq!(rolled, 3, "digest rolls up under mda_app too");

    let digested: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sys_notification WHERE tenant_id=$1 AND digested_at IS NOT NULL",
    )
    .bind(ctx.tenant)
    .fetch_one(&ctx.pool)
    .await
    .unwrap();
    assert_eq!(digested, 3);
}

/// `resolve_record_readers` walks `sec_team.parent_id` UP from the owner's
/// team. A cycle in that graph (historically creatable via import) must not
/// hang the walk: the recursive term deduplicates (`UNION`), like every other
/// hierarchy walk since HARDENING pass 3.
#[tokio::test]
async fn record_reader_resolution_terminates_on_team_parent_cycle() {
    let Some(ctx) = setup().await else {
        return;
    };
    let mut tx = ctx.pool.begin().await.unwrap();
    mda_security::set_tenant(&mut tx, ctx.tenant).await.unwrap();
    // team-OWD on some entity so the ancestor-team branch runs at all.
    sqlx::query(
        "INSERT INTO sec.sec_owd (tenant_id, entity, default_access) VALUES ($1,'Customer','team')",
    )
    .bind(ctx.tenant)
    .execute(&mut *tx)
    .await
    .unwrap();
    // two teams whose parent links form a cycle (direct SQL = import-era data).
    let (t1,): (Uuid,) =
        sqlx::query_as("INSERT INTO sec.sec_team (tenant_id, name) VALUES ($1,'A') RETURNING id")
            .bind(ctx.tenant)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    let (t2,): (Uuid,) = sqlx::query_as(
        "INSERT INTO sec.sec_team (tenant_id, name, parent_id) VALUES ($1,'B',$2) RETURNING id",
    )
    .bind(ctx.tenant)
    .bind(t1)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    sqlx::query("UPDATE sec.sec_team SET parent_id = $2 WHERE id = $1")
        .bind(t1)
        .bind(t2)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE sec.sec_user SET team_id = $2 WHERE id = $1")
        .bind(ctx.user_id)
        .bind(t1)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    // must terminate and resolve the owner (an object-level reader via */*)
    // through the cyclic ancestor walk.
    let readers = mda_api::notifications::resolve_record_readers(
        &ctx.pool,
        ctx.tenant,
        "Customer",
        ctx.user_id,
        Uuid::new_v4(),
    )
    .await
    .unwrap();
    assert!(
        readers.contains(&ctx.user_id),
        "owner resolved despite the cycle: {readers:?}"
    );
}

/// A failed outbox row must be retried after its backoff and dead-lettered
/// after [`mda_server::outbox::MAX_RETRIES`] failures — not stranded forever
/// (the pre-fix drain claimed only `status='pending'`; a single transient
/// failure permanently parked the side-effect and §5.9.4's DLQ was
/// unreachable).
#[tokio::test]
async fn failed_outbox_rows_are_retried_then_dead_lettered() {
    use std::sync::Arc;
    let Some(ctx) = setup().await else {
        return;
    };
    // poison row: a fanout payload with no tenant_id fails deterministically.
    let (row_id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO sys_outbox (tenant_id, kind, payload) VALUES ($1,'notification.fanout','{}')
         RETURNING id",
    )
    .bind(ctx.tenant)
    .fetch_one(&ctx.pool)
    .await
    .unwrap();

    let channels = mda_api::notifications::default_channels();
    let secrets: Arc<dyn mda_core::SecretStore> =
        Arc::new(mda_api::secrets::LocalSecretStore::from_env());
    let http = reqwest::Client::new();
    async fn status_of(pool: &PgPool, row_id: Uuid) -> (String, i32) {
        let (status, attempts): (String, i32) =
            sqlx::query_as("SELECT status, attempts FROM sys_outbox WHERE id = $1")
                .bind(row_id)
                .fetch_one(pool)
                .await
                .unwrap();
        (status, attempts)
    }

    // pass 1: fails → 'failed', attempts 1, backoff stamped.
    mda_server::outbox::drain_once(&ctx.pool, &channels, secrets.as_ref(), &http)
        .await
        .unwrap();
    let (st, n) = status_of(&ctx.pool, row_id).await;
    assert_eq!((st.as_str(), n), ("failed", 1));

    // immediate second pass: the backoff holds the row (no hot retry loop).
    mda_server::outbox::drain_once(&ctx.pool, &channels, secrets.as_ref(), &http)
        .await
        .unwrap();
    let (st, n) = status_of(&ctx.pool, row_id).await;
    assert_eq!((st.as_str(), n), ("failed", 1), "backoff defers the retry");

    // age out the backoff → the row is claimed again and fails again.
    for expected_attempts in 2..=mda_server::outbox::MAX_RETRIES {
        sqlx::query("UPDATE sys_outbox SET processed_at = now() - interval '1 hour' WHERE id=$1")
            .bind(row_id)
            .execute(&ctx.pool)
            .await
            .unwrap();
        mda_server::outbox::drain_once(&ctx.pool, &channels, secrets.as_ref(), &http)
            .await
            .unwrap();
        let (st, n) = status_of(&ctx.pool, row_id).await;
        assert_eq!(n, expected_attempts, "attempt {expected_attempts} ran");
        if st == "dead" {
            break;
        }
        assert_eq!(st, "failed");
    }
    let (st, n) = status_of(&ctx.pool, row_id).await;
    assert_eq!(st, "dead", "dead-letter reached after exhausting retries");
    assert_eq!(n, mda_server::outbox::MAX_RETRIES);

    // dead is terminal: even with the backoff fully elapsed, a dead-lettered
    // row is never claimed again (no zombie deliveries from the DLQ).
    sqlx::query("UPDATE sys_outbox SET processed_at = now() - interval '1 hour' WHERE id=$1")
        .bind(row_id)
        .execute(&ctx.pool)
        .await
        .unwrap();
    mda_server::outbox::drain_once(&ctx.pool, &channels, secrets.as_ref(), &http)
        .await
        .unwrap();
    let (st, n) = status_of(&ctx.pool, row_id).await;
    assert_eq!(
        (st.as_str(), n),
        ("dead", mda_server::outbox::MAX_RETRIES),
        "dead rows are never re-claimed"
    );
}

/// Rows parked at `status='failed'` by the PRE-retry drain (which never
/// stamped `processed_at` on failure) must be rescued by the new code, not
/// stranded forever: a failed row with a NULL last-attempt timestamp counts
/// as backoff-elapsed and is retried once immediately (the attempt then
/// stamps the timestamp, so normal backoff takes over). Without the
/// NULL-tolerant claim arm, every failed row in an upgraded database would
/// be unclaimable for eternity.
#[tokio::test]
async fn legacy_failed_rows_without_a_processed_at_are_rescued() {
    use std::sync::Arc;
    let Some(ctx) = setup().await else {
        return;
    };
    // the exact shape the pre-retry drain left behind: failed, attempts
    // counted, processed_at never stamped.
    let (row_id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO sys_outbox (tenant_id, kind, payload, status, attempts)
         VALUES ($1,'notification.fanout','{}','failed',3)
         RETURNING id",
    )
    .bind(ctx.tenant)
    .fetch_one(&ctx.pool)
    .await
    .unwrap();

    let channels = mda_api::notifications::default_channels();
    let secrets: Arc<dyn mda_core::SecretStore> =
        Arc::new(mda_api::secrets::LocalSecretStore::from_env());
    let http = reqwest::Client::new();

    mda_server::outbox::drain_once(&ctx.pool, &channels, secrets.as_ref(), &http)
        .await
        .unwrap();
    let (st, n, stamped): (String, i32, bool) = sqlx::query_as(
        "SELECT status, attempts, processed_at IS NOT NULL FROM sys_outbox WHERE id=$1",
    )
    .bind(row_id)
    .fetch_one(&ctx.pool)
    .await
    .unwrap();
    assert_eq!(
        (st.as_str(), n, stamped),
        ("failed", 4, true),
        "legacy row retried immediately and stamped for backoff"
    );
}

/// At-least-once delivery means the drain may run the SAME row twice — a
/// worker crash between delivering and stamping the row leaves it claimable
/// again (simulated here by resetting a done row to pending). Every durable
/// row a channel writes derives its id from the outbox row id, so the replay
/// must be a NO-OP: no duplicate in-app notification, no duplicate email
/// record, no duplicate `notification.created` event.
#[tokio::test]
async fn replayed_outbox_rows_do_not_duplicate_deliveries() {
    use std::sync::Arc;
    let Some(ctx) = setup().await else {
        return;
    };
    // a registered type delivering on both channels.
    call(
        &ctx.app,
        "POST",
        "/api/notification-types",
        &ctx.token,
        Some(
            json!({"key":"invoice.overdue","label":"Overdue",
                   "default_channels":["in_app","email"]})
            .to_string(),
        ),
    )
    .await;
    let (row_id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO sys_outbox (tenant_id, kind, payload)
         VALUES ($1,'notification.fanout',$2)
         RETURNING id",
    )
    .bind(ctx.tenant)
    .bind(
        json!({"tenant_id": ctx.tenant, "type_key": "invoice.overdue",
                 "recipients": [ctx.user_id], "context": {"record": {"name": "Acme"}}}),
    )
    .fetch_one(&ctx.pool)
    .await
    .unwrap();

    let channels = mda_api::notifications::default_channels();
    let secrets: Arc<dyn mda_core::SecretStore> =
        Arc::new(mda_api::secrets::LocalSecretStore::from_env());
    let http = reqwest::Client::new();
    async fn counts(pool: &PgPool, tenant: Uuid, user: Uuid) -> (i64, i64, i64) {
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM sys_notification WHERE tenant_id=$1 AND user_id=$2",
        )
        .bind(tenant)
        .bind(user)
        .fetch_one(pool)
        .await
        .unwrap();
        let m: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM sys_message WHERE tenant_id=$1 AND user_id=$2",
        )
        .bind(tenant)
        .bind(user)
        .fetch_one(pool)
        .await
        .unwrap();
        let e: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM sys_event_log WHERE tenant_id=$1 AND type='notification.created'",
        )
        .bind(tenant)
        .fetch_one(pool)
        .await
        .unwrap();
        (n, m, e)
    }

    // first pass: delivers on both channels, marks the row done.
    mda_server::outbox::drain_once(&ctx.pool, &channels, secrets.as_ref(), &http)
        .await
        .unwrap();
    let (st, att): (String, i32) =
        sqlx::query_as("SELECT status, attempts FROM sys_outbox WHERE id = $1")
            .bind(row_id)
            .fetch_one(&ctx.pool)
            .await
            .unwrap();
    assert_eq!((st.as_str(), att), ("done", 0), "row delivered and done");
    let after_first = counts(&ctx.pool, ctx.tenant, ctx.user_id).await;
    assert_eq!(
        after_first,
        (1, 1, 1),
        "one notification, one message, one event"
    );

    // crash-before-stamp: the row goes back to claimable and is replayed.
    sqlx::query(
        "UPDATE sys_outbox SET status='pending', attempts=0, processed_at=NULL WHERE id=$1",
    )
    .bind(row_id)
    .execute(&ctx.pool)
    .await
    .unwrap();
    mda_server::outbox::drain_once(&ctx.pool, &channels, secrets.as_ref(), &http)
        .await
        .unwrap();
    let after_replay = counts(&ctx.pool, ctx.tenant, ctx.user_id).await;
    assert_eq!(
        after_first, after_replay,
        "replay of the same outbox row delivered nothing new"
    );
}
