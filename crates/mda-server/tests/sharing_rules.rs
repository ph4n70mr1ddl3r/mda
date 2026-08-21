//! Criteria-based sharing rules + role hierarchy (ADR-0013 closure): the
//! materialized-visibility half of record security. Covers rule CRUD, the
//! synchronous per-record recompute in the write path (grant AND revoke), the
//! epoch-gated enforcement (a narrowing rule edit revokes instantly), team
//! principals, the resumable recompute endpoint, and the live role-hierarchy
//! read amplification (read-only — never write).

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

struct Ctx {
    app: axum::Router,
    admin_token: String,
    pool: PgPool,
    tenant: Uuid,
    admin_id: Uuid,
}

fn deal_model(table: &str) -> Value {
    json!({
        "modules": [],
        "entities": [{
            "id": Uuid::new_v4(), "module_id": null, "name": "Deal",
            "table_name": table, "label": "Deal", "description": null,
            "fields": [
                {"id": Uuid::new_v4(), "name":"name","label":"Name","field_type":"string","required":true,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{}},
                {"id": Uuid::new_v4(), "name":"amount","label":"Amount","field_type":"integer","required":false,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{}}
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
    let email = format!("a{}@test", Uuid::new_v4().simple());
    let admin_id = common::seed_user(&pool, tenant, &email, "admin", &hash).await;
    common::seed_assignment(&pool, tenant, admin_id, role_id).await;
    let jwt = JwtConfig::from_env();
    let admin_token = jwt.issue_access(admin_id, tenant, None).unwrap();
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
        gql: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
    });
    Some(Ctx {
        app,
        admin_token,
        pool,
        tenant,
        admin_id,
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
        &ctx.admin_token,
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
            format!("Bearer {}", ctx.admin_token),
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
        &ctx.admin_token,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
}

/// A user with (optionally restricted) Deal permissions, returning (id, token).
async fn seed_user(ctx: &Ctx, perms: &[(&str, &str)]) -> (Uuid, String) {
    let hash = mda_security::hash_password("x").unwrap();
    let email = format!("u{}@test", Uuid::new_v4().simple());
    let id = common::seed_user(&ctx.pool, ctx.tenant, &email, "u", &hash).await;
    if !perms.is_empty() {
        let role = common::seed_role(
            &ctx.pool,
            ctx.tenant,
            &format!("r{}", Uuid::new_v4().simple()),
            perms,
        )
        .await;
        common::seed_assignment(&ctx.pool, ctx.tenant, id, role).await;
    }
    let jwt = JwtConfig::from_env();
    let token = jwt.issue_access(id, ctx.tenant, None).unwrap();
    (id, token)
}

async fn create_deal(ctx: &Ctx, name: &str, amount: i64) -> Uuid {
    let (st, v) = call(
        &ctx.app,
        "POST",
        "/api/data/Deal",
        &ctx.admin_token,
        Some(json!({"name": name, "amount": amount}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "create deal: {v}");
    v["id"].as_str().unwrap().parse().unwrap()
}

async fn reader_sees(ctx: &Ctx, token: &str, id: Uuid) -> bool {
    let (st, _) = call(
        &ctx.app,
        "GET",
        &format!("/api/data/Deal/{id}"),
        token,
        None,
    )
    .await;
    st == StatusCode::OK
}

#[tokio::test]
async fn sharing_rule_grants_and_revokes_per_record() {
    let Some(ctx) = setup().await else { return };
    publish(
        &ctx,
        deal_model(&format!("deal_{}", Uuid::new_v4().simple())),
    )
    .await;
    let (reader_id, reader_token) = seed_user(&ctx, &[("Deal", "read")]).await;

    // records exist BEFORE the rule: creation materializes nothing for reader yet
    let low = create_deal(&ctx, "low", 50).await;
    let high = create_deal(&ctx, "high", 150).await;
    assert!(!reader_sees(&ctx, &reader_token, high).await, "no rule yet");

    // rule: amount >= 100 shared with reader (read)
    let cond = json!({
        "op":"Cmp","kind":"ge",
        "lhs":{"op":"Field","name":"amount"},
        "rhs":{"op":"Lit","value":100}
    });
    let (st, v) = call(
        &ctx.app,
        "POST",
        "/api/admin/share-rules",
        &ctx.admin_token,
        Some(
            json!({"entity":"Deal","condition":cond,"principal_id":reader_id,"access":"read"})
                .to_string(),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "create rule: {v}");
    assert_eq!(
        v["recompute"]["scanned"], 2,
        "materialized existing rows: {v}"
    );
    assert_eq!(v["recompute"]["materialized"], 1);
    let rule_id = v["rule"]["id"].as_str().unwrap().parse::<Uuid>().unwrap();

    assert!(reader_sees(&ctx, &reader_token, high).await, "rule grants");
    assert!(
        !reader_sees(&ctx, &reader_token, low).await,
        "non-matching record stays invisible"
    );

    // a NEW matching record is materialized synchronously in its write txn
    let big = create_deal(&ctx, "big", 500).await;
    assert!(
        reader_sees(&ctx, &reader_token, big).await,
        "create materializes"
    );

    // narrowing the record revokes synchronously — no admin action needed
    let version = st_to_version(&ctx, big).await;
    let req = Request::builder()
        .method("PATCH")
        .uri(format!("/api/data/Deal/{big}"))
        .header(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {}", ctx.admin_token),
        )
        .header("if-match", version.to_string())
        .header("content-type", "application/json")
        .body(Body::from(json!({"amount": 5}).to_string()))
        .unwrap();
    let resp = ctx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        !reader_sees(&ctx, &reader_token, big).await,
        "per-record revoke is synchronous"
    );

    // widening the record re-grants synchronously too
    let version = st_to_version(&ctx, big).await;
    let req = Request::builder()
        .method("PATCH")
        .uri(format!("/api/data/Deal/{big}"))
        .header(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {}", ctx.admin_token),
        )
        .header("if-match", version.to_string())
        .header("content-type", "application/json")
        .body(Body::from(json!({"amount": 900}).to_string()))
        .unwrap();
    let resp = ctx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(reader_sees(&ctx, &reader_token, big).await, "re-grant");

    // ---- narrowing RULE edit: epoch bump revokes instantly ----
    let (st, v) = call(
        &ctx.app,
        "PATCH",
        &format!("/api/admin/share-rules/{rule_id}"),
        &ctx.admin_token,
        Some(
            json!({"condition": {
                "op":"Cmp","kind":"ge",
                "lhs":{"op":"Field","name":"amount"},
                "rhs":{"op":"Lit","value":1000}
            }})
            .to_string(),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "edit rule: {v}");
    assert!(
        !reader_sees(&ctx, &reader_token, high).await,
        "epoch bump revoked instantly"
    );
    // widen back via recompute-able edit
    let (st, v) = call(
        &ctx.app,
        "POST",
        &format!("/api/admin/share-rules/{rule_id}/recompute?limit=100"),
        &ctx.admin_token,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "recompute: {v}");
    assert_eq!(v["materialized"], 0, "nothing matches >=1000: {v}");

    // deactivate: revoke everything this rule granted
    let (st, _) = call(
        &ctx.app,
        "PATCH",
        &format!("/api/admin/share-rules/{rule_id}"),
        &ctx.admin_token,
        Some(json!({"active": false}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(!reader_sees(&ctx, &reader_token, big).await, "deactivated");

    // delete the rule: 204, still revoked
    let (st, _) = call(
        &ctx.app,
        "DELETE",
        &format!("/api/admin/share-rules/{rule_id}"),
        &ctx.admin_token,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
}

async fn st_to_version(ctx: &Ctx, id: Uuid) -> i64 {
    let (_, v) = call(
        &ctx.app,
        "GET",
        &format!("/api/data/Deal/{id}"),
        &ctx.admin_token,
        None,
    )
    .await;
    v["version"].as_i64().unwrap()
}

#[tokio::test]
async fn share_rule_team_principal_matches_members() {
    let Some(ctx) = setup().await else { return };
    publish(
        &ctx,
        deal_model(&format!("deal_{}", Uuid::new_v4().simple())),
    )
    .await;
    let (reader_id, reader_token) = seed_user(&ctx, &[("Deal", "read")]).await;

    // a team; reader is a member
    let (team_id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO sec.sec_team (tenant_id, name) VALUES ($1, 'field-sales') RETURNING id",
    )
    .bind(ctx.tenant)
    .fetch_one(&ctx.pool)
    .await
    .unwrap();
    let mut tx = ctx.pool.begin().await.unwrap();
    mda_security::set_tenant(&mut tx, ctx.tenant).await.unwrap();
    sqlx::query("UPDATE sec.sec_user SET team_id = $1 WHERE id = $2")
        .bind(team_id)
        .bind(reader_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let deal = create_deal(&ctx, "enterprise", 10_000).await;
    let (st, _) = call(
        &ctx.app,
        "POST",
        "/api/admin/share-rules",
        &ctx.admin_token,
        Some(
            json!({
                "entity":"Deal",
                "condition": {"op":"Cmp","kind":"ge","lhs":{"op":"Field","name":"amount"},"rhs":{"op":"Lit","value":5000}},
                "principal_id": team_id,
                "access": "read"
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    assert!(
        reader_sees(&ctx, &reader_token, deal).await,
        "team-principal rule grants members"
    );
}

#[tokio::test]
async fn role_hierarchy_reads_but_never_writes_subordinate_records() {
    let Some(ctx) = setup().await else { return };
    publish(
        &ctx,
        deal_model(&format!("deal_{}", Uuid::new_v4().simple())),
    )
    .await;

    // manager role is parented above rep role
    let manager_role = common::seed_role(
        &ctx.pool,
        ctx.tenant,
        "manager",
        &[("Deal", "read"), ("Deal", "update")],
    )
    .await;
    let rep_role = common::seed_role(
        &ctx.pool,
        ctx.tenant,
        "rep",
        &[("Deal", "read"), ("Deal", "update"), ("Deal", "create")],
    )
    .await;
    let (manager_id, manager_token) = seed_user(&ctx, &[]).await;
    let (rep_id, rep_token) = seed_user(&ctx, &[]).await;
    common::seed_assignment(&ctx.pool, ctx.tenant, manager_id, manager_role).await;
    common::seed_assignment(&ctx.pool, ctx.tenant, rep_id, rep_role).await;

    let (st, _) = call(
        &ctx.app,
        "POST",
        &format!("/api/admin/roles/{rep_role}/parents/{manager_role}"),
        &ctx.admin_token,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "parent rep under manager");

    // the parents listing is key-addressable JSON ({id, name} objects),
    // which is what the Studio's role-hierarchy panel parses
    let (st, v) = call(
        &ctx.app,
        "GET",
        &format!("/api/admin/roles/{rep_role}/parents"),
        &ctx.admin_token,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["parents"].as_array().unwrap().len(), 1);
    assert_eq!(
        v["parents"][0]["id"].as_str().unwrap(),
        manager_role.to_string()
    );
    assert_eq!(v["parents"][0]["name"].as_str().unwrap(), "manager");

    // rep owns a private record
    let (st, v) = call(
        &ctx.app,
        "POST",
        "/api/data/Deal",
        &rep_token,
        Some(json!({"name":"rep's deal","amount":7}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let deal = v["id"].as_str().unwrap().parse::<Uuid>().unwrap();

    // the manager READS it (live hierarchy) but cannot WRITE it
    assert!(
        reader_sees(&ctx, &manager_token, deal).await,
        "manager reads down"
    );
    let version = {
        let (_, v) = call(
            &ctx.app,
            "GET",
            &format!("/api/data/Deal/{deal}"),
            &manager_token,
            None,
        )
        .await;
        v["version"].as_i64().unwrap()
    };
    let req = Request::builder()
        .method("PATCH")
        .uri(format!("/api/data/Deal/{deal}"))
        .header(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {manager_token}"),
        )
        .header("if-match", version.to_string())
        .header("content-type", "application/json")
        .body(Body::from(json!({"amount": 8}).to_string()))
        .unwrap();
    let resp = ctx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "hierarchy is read-only — no write amplification (404, no leak)"
    );

    // detach the parent: instant revoke (live evaluation, no lag)
    let (st, _) = call(
        &ctx.app,
        "DELETE",
        &format!("/api/admin/roles/{rep_role}/parents/{manager_role}"),
        &ctx.admin_token,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    assert!(
        !reader_sees(&ctx, &manager_token, deal).await,
        "detach revokes immediately"
    );

    // cycle guard
    let (st, _) = call(
        &ctx.app,
        "POST",
        &format!("/api/admin/roles/{manager_role}/parents/{rep_role}"),
        &ctx.admin_token,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let (st, v) = call(
        &ctx.app,
        "POST",
        &format!("/api/admin/roles/{rep_role}/parents/{manager_role}"),
        &ctx.admin_token,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "cycle rejected: {v}");
}

#[tokio::test]
async fn share_rule_management_requires_admin() {
    let Some(ctx) = setup().await else { return };
    publish(
        &ctx,
        deal_model(&format!("deal_{}", Uuid::new_v4().simple())),
    )
    .await;
    let (_, plain_token) = seed_user(&ctx, &[("Deal", "read")]).await;
    let (st, _) = call(
        &ctx.app,
        "GET",
        "/api/admin/share-rules",
        &plain_token,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "non-admin cannot list rules");
    let (st, _) = call(
        &ctx.app,
        "POST",
        "/api/admin/share-rules",
        &plain_token,
        Some(
            json!({
                "entity":"Deal",
                "condition": {"op":"Lit","value":true},
                "principal_id": ctx.admin_id,
                "access": "read"
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "non-admin cannot create rules");
}

#[tokio::test]
async fn write_share_lets_a_non_owner_delete() {
    // Exercises the write predicate on the DELETE path (a latent bug: the old
    // SQL emitted an unaliased `t.` reference there, so any non-superuser
    // delete failed with a 500 instead of honoring write shares).
    let Some(ctx) = setup().await else { return };
    publish(
        &ctx,
        deal_model(&format!("deal_{}", Uuid::new_v4().simple())),
    )
    .await;
    let (writer_id, writer_token) = seed_user(&ctx, &[("Deal", "read"), ("Deal", "delete")]).await;

    let deal = create_deal(&ctx, "temporary", 1).await;
    let (st, _) = call(
        &ctx.app,
        "POST",
        "/api/admin/share-rules",
        &ctx.admin_token,
        Some(
            json!({
                "entity":"Deal",
                "condition": {"op":"Lit","value":true},
                "principal_id": writer_id,
                "access": "write"
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);

    let (st, _) = call(
        &ctx.app,
        "DELETE",
        &format!("/api/data/Deal/{deal}"),
        &writer_token,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT, "write share grants delete");
    // gone for the owner too (and its shares were cleaned up)
    let (st, _) = call(
        &ctx.app,
        "GET",
        &format!("/api/data/Deal/{deal}"),
        &ctx.admin_token,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (n,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM sec.sec_record_share WHERE tenant_id = $1 AND record_id = $2",
    )
    .bind(ctx.tenant)
    .bind(deal)
    .fetch_one(&ctx.pool)
    .await
    .unwrap();
    assert_eq!(n, 0, "hard delete drops materialized shares");
}
