//! Mass actions (PLAN §9 deferral): bulk update / delete **by filter**, distinct
//! from the §5.13 file import (which is row-by-row from a file). Each affected
//! record goes through the normal single-record write pipeline — RBAC, FLS
//! write-check, rules, calculated fields, OCC, audit, and the event log — so a
//! mass update is indistinguishable from N hand-typed PATCHes and respects
//! record-level security on every row. A hard cap bounds the blast radius;
//! `dry_run` returns the candidate id set without mutating.

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
    pool: PgPool,
    tenant: Uuid,
}

fn customer_model() -> Value {
    json!({
        "modules": [],
        "entities": [{
            "id": Uuid::new_v4(),
            "module_id": null,
            "name": "Customer",
            "table_name": format!("cust_{}", Uuid::new_v4().simple()),
            "label": "Customer",
            "description": null,
            "fields": [
                {"id": Uuid::new_v4(), "name":"name","label":"Name","field_type":"string","required":true,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{}},
                {"id": Uuid::new_v4(), "name":"tier","label":"Tier","field_type":"enum","required":false,"is_unique":false,"is_indexed":true,"default_expr":null,"config":{"options":["Bronze","Silver","Gold"]}},
                {"id": Uuid::new_v4(), "name":"balance","label":"Balance","field_type":"decimal","required":false,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{"precision":12,"scale":2}}
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
    let etag = d["version_etag"].as_str().unwrap().to_string();
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
        None,
    )
    .await;
}

async fn create(ctx: &Ctx, name: &str, tier: &str, balance: f64) -> Uuid {
    let (_, rec) = call(
        &ctx.app,
        "POST",
        "/api/data/Customer",
        &ctx.token,
        Some(json!({"name":name,"tier":tier,"balance":balance}).to_string()),
    )
    .await;
    rec["id"].as_str().unwrap().parse().unwrap()
}

#[tokio::test]
async fn mass_update_by_filter_applies_and_audits() {
    let Some(ctx) = setup().await else {
        return;
    };
    publish(&ctx, customer_model()).await;

    // 3 Bronze, 1 Gold.
    create(&ctx, "A", "Bronze", 10.0).await;
    create(&ctx, "B", "Bronze", 20.0).await;
    create(&ctx, "C", "Bronze", 30.0).await;
    create(&ctx, "D", "Gold", 999.0).await;

    // dry-run: reports the 3 Bronze candidates without changing anything.
    let (st, res) = call(
        &ctx.app,
        "POST",
        "/api/data/Customer/mass-update",
        &ctx.token,
        Some(
            json!({
                "filter": ["tier:eq:Bronze"],
                "set": {"tier": "Silver"},
                "dry_run": true,
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "dry-run: {res}");
    assert_eq!(res["dry_run"], true);
    assert_eq!(res["affected"], 3, "dry-run affected: {res}");
    assert_eq!(res["ids"].as_array().unwrap().len(), 3);

    // nothing changed yet
    let (_, list) = call(
        &ctx.app,
        "GET",
        "/api/data/Customer?filter=tier:eq:Silver",
        &ctx.token,
        None,
    )
    .await;
    assert_eq!(list["total"], 0, "dry-run must not mutate");

    // real run: promote all Bronze → Silver
    let (st, res) = call(
        &ctx.app,
        "POST",
        "/api/data/Customer/mass-update",
        &ctx.token,
        Some(
            json!({
                "filter": ["tier:eq:Bronze"],
                "set": {"tier": "Silver"},
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "mass-update: {res}");
    assert_eq!(res["affected"], 3, "affected: {res}");
    assert_eq!(res["dry_run"], false);
    assert!(res["errors"].as_array().unwrap().is_empty(), "{res}");

    // verify via list: 3 Silver now, 0 Bronze, 1 Gold
    let (_, silver) = call(
        &ctx.app,
        "GET",
        "/api/data/Customer?filter=tier:eq:Silver",
        &ctx.token,
        None,
    )
    .await;
    assert_eq!(silver["total"], 3, "3 promoted: {silver}");
    let (_, gold) = call(
        &ctx.app,
        "GET",
        "/api/data/Customer?filter=tier:eq:Gold",
        &ctx.token,
        None,
    )
    .await;
    assert_eq!(gold["total"], 1, "Gold untouched");

    // audit + event log reflect 3 updates (per-record, full parity).
    let updates: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sys_audit_log WHERE tenant_id = $1 AND entity='Customer' AND op='update'",
    )
    .bind(ctx.tenant)
    .fetch_one(&ctx.pool)
    .await
    .unwrap();
    assert_eq!(
        updates, 3,
        "one audit row per updated record: got {updates}"
    );
    let events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sys_event_log WHERE tenant_id = $1 AND entity='Customer' AND type='record.updated'",
    )
    .bind(ctx.tenant)
    .fetch_one(&ctx.pool)
    .await
    .unwrap();
    assert_eq!(events, 3, "one event per updated record: got {events}");

    // each updated record's version advanced (OCC increments per record).
    let (_, one) = call(
        &ctx.app,
        "GET",
        "/api/data/Customer?filter=tier:eq:Silver&page_size=1",
        &ctx.token,
        None,
    )
    .await;
    assert_eq!(one["items"][0]["version"], 2, "version advanced to 2");
}

#[tokio::test]
async fn mass_delete_by_filter_removes_and_audits() {
    let Some(ctx) = setup().await else {
        return;
    };
    publish(&ctx, customer_model()).await;
    create(&ctx, "A", "Bronze", 10.0).await;
    create(&ctx, "B", "Bronze", 20.0).await;
    create(&ctx, "C", "Gold", 30.0).await;

    // delete all Bronze (2 records); Gold survives.
    let (st, res) = call(
        &ctx.app,
        "POST",
        "/api/data/Customer/mass-delete",
        &ctx.token,
        Some(json!({ "filter": ["tier:eq:Bronze"] }).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "mass-delete: {res}");
    assert_eq!(res["affected"], 2, "{res}");

    let (_, list) = call(&ctx.app, "GET", "/api/data/Customer", &ctx.token, None).await;
    assert_eq!(list["total"], 1, "only Gold remains: {list}");
    assert_eq!(list["items"][0]["tier"], "Gold");

    // 2 delete audit rows + 2 record.deleted events.
    let del_audit: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sys_audit_log WHERE tenant_id=$1 AND entity='Customer' AND op='delete'",
    )
    .bind(ctx.tenant)
    .fetch_one(&ctx.pool)
    .await
    .unwrap();
    assert_eq!(del_audit, 2);
    let del_events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sys_event_log WHERE tenant_id=$1 AND entity='Customer' AND type='record.deleted'",
    )
    .bind(ctx.tenant)
    .fetch_one(&ctx.pool)
    .await
    .unwrap();
    assert_eq!(del_events, 2);
}

async fn make_user(ctx: &Ctx, name: &str, perms: &[(&str, &str)]) -> (String, Uuid) {
    let role_id = common::seed_role(
        &ctx.pool,
        ctx.tenant,
        &format!("r{}", Uuid::new_v4().simple()),
        perms,
    )
    .await;
    let hash = mda_security::hash_password("x").unwrap();
    let email = format!("u{}@test", Uuid::new_v4().simple());
    let uid = common::seed_user(&ctx.pool, ctx.tenant, &email, name, &hash).await;
    common::seed_assignment(&ctx.pool, ctx.tenant, uid, role_id).await;
    (
        JwtConfig::from_env()
            .issue_access(uid, ctx.tenant, None)
            .unwrap(),
        uid,
    )
}

#[tokio::test]
async fn mass_action_respects_rbac_and_record_scope() {
    let Some(ctx) = setup().await else {
        return;
    };
    publish(&ctx, customer_model()).await;

    // Two NON-superuser users, each with read+create+update (no delete). Neither
    // has the ("*","*") grant, so record-level scope is enforced for both.
    let perms = &[
        ("Customer", "read"),
        ("Customer", "create"),
        ("Customer", "update"),
    ];
    let (owner_token, _owner) = make_user(&ctx, "owner", perms).await;
    let (other_token, _other) = make_user(&ctx, "other", perms).await;

    // The owner creates two private Bronze records.
    for n in ["L1", "L2"] {
        call(
            &ctx.app,
            "POST",
            "/api/data/Customer",
            &owner_token,
            Some(json!({"name":n,"tier":"Bronze"}).to_string()),
        )
        .await;
    }

    // RBAC: mass-delete is denied (neither has `delete`) → 403.
    let (st, _) = call(
        &ctx.app,
        "POST",
        "/api/data/Customer/mass-delete",
        &owner_token,
        Some(json!({"filter":["tier:eq:Bronze"]}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "RBAC denies mass-delete");

    // Record scope: the OTHER user's mass-update on tier=Bronze matches 0 — the
    // owner's private records are invisible/unwritable to them (OWD = private).
    let (st, res) = call(
        &ctx.app,
        "POST",
        "/api/data/Customer/mass-update",
        &other_token,
        Some(json!({"filter":["tier:eq:Bronze"],"set":{"tier":"Silver"}}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{res}");
    assert_eq!(
        res["affected"], 0,
        "other user cannot mass-update the owner's private records: {res}"
    );

    // The owner's own mass-update touches exactly their 2 records.
    let (st, res) = call(
        &ctx.app,
        "POST",
        "/api/data/Customer/mass-update",
        &owner_token,
        Some(json!({"filter":["tier:eq:Bronze"],"set":{"tier":"Silver"}}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{res}");
    assert_eq!(res["affected"], 2, "owner updates their own records");
    let (_, list) = call(
        &ctx.app,
        "GET",
        "/api/data/Customer?filter=tier:eq:Silver",
        &owner_token,
        None,
    )
    .await;
    assert_eq!(list["total"], 2, "both promoted");
}

#[tokio::test]
async fn mass_update_respects_field_level_security() {
    let Some(ctx) = setup().await else {
        return;
    };
    publish(&ctx, customer_model()).await;

    // A user who can update Customer but has NO write on `balance` (FLS).
    let role_id = common::seed_role(
        &ctx.pool,
        ctx.tenant,
        &format!("r{}", Uuid::new_v4().simple()),
        &[
            ("Customer", "read"),
            ("Customer", "create"),
            ("Customer", "update"),
        ],
    )
    .await;
    {
        let mut tx = ctx.pool.begin().await.unwrap();
        mda_security::set_tenant(&mut tx, ctx.tenant).await.unwrap();
        sqlx::query(
            "INSERT INTO sec.sec_field_permission (role_id, entity, field, access) \
             VALUES ($1,'Customer','balance','read')",
        )
        .bind(role_id)
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }
    let hash = mda_security::hash_password("x").unwrap();
    let email = format!("u{}@test", Uuid::new_v4().simple());
    let lim = common::seed_user(&ctx.pool, ctx.tenant, &email, "lim", &hash).await;
    common::seed_assignment(&ctx.pool, ctx.tenant, lim, role_id).await;
    let lim_token = JwtConfig::from_env()
        .issue_access(lim, ctx.tenant, None)
        .unwrap();

    // The limited user owns a record.
    call(
        &ctx.app,
        "POST",
        "/api/data/Customer",
        &lim_token,
        Some(json!({"name":"X","tier":"Bronze","balance":5.0}).to_string()),
    )
    .await;

    // mass-update that tries to write `balance` → the whole action is rejected
    // (the FLS write-check fails for the patch before any record is touched).
    let (st, res) = call(
        &ctx.app,
        "POST",
        "/api/data/Customer/mass-update",
        &lim_token,
        Some(json!({"filter":["tier:eq:Bronze"],"set":{"balance":999.0}}).to_string()),
    )
    .await;
    assert!(
        !st.is_success(),
        "mass-update writing a non-writable field must fail: {st} {res}"
    );
    assert_eq!(res["code"], "mda.forbidden", "{res}");
}
