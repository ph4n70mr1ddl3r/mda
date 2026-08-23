//! Many-to-many runtime behavior (§5.7): a `many_to_many` relationship
//! materializes a join table at publish — the record's own `biz.<table>` row
//! has **no** hoisted FK column for it. Verifies CRUD on an m2m-bearing
//! entity works (create/update/list/restore address only real columns) and
//! that addressing the m2m field in a record body / filter / sort is a clean
//! 422, never a 500 over a nonexistent column.

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
    version: Option<i64>,
) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(uri);
    b = b.header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"));
    if let Some(v) = version {
        b = b.header("if-match", format!("\"{v}\""));
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

/// Customer ──many_to_many──▶ Tag (join table materialized at publish).
fn m2m_model() -> Value {
    let customer_id = Uuid::new_v4();
    let tag_id = Uuid::new_v4();
    // short table-name suffixes: the m2m join table name is derived from BOTH
    // table names (`<a>_<b>`), and a Postgres identifier caps at 63 bytes —
    // full uuid suffixes would overflow it (the draft gate correctly rejects
    // that; a short suffix keeps this fixture publishable).
    let cs: String = customer_id.simple().to_string().chars().take(6).collect();
    let ts: String = tag_id.simple().to_string().chars().take(6).collect();
    json!({
        "modules": [],
        "entities": [
            {
                "id": customer_id, "module_id": null, "name": "Customer",
                "table_name": format!("cust_{cs}"),
                "label": "Customer", "description": null,
                "fields": [
                    {"id": Uuid::new_v4(), "name":"name","label":"Name","field_type":"string","required":true,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{}}
                ],
                "relationships": [
                    {"id": Uuid::new_v4(), "source_field_name":"tags","target_entity_id": tag_id,
                     "cardinality":"many_to_many","strength":"lookup","on_delete":null,
                     "required":false,"reference_qualifier":null,"rollup_summary":null,"join_table_name":null}
                ]
            },
            {
                "id": tag_id, "module_id": null, "name": "Tag",
                "table_name": format!("tag_{ts}"),
                "label": "Tag", "description": null,
                "fields": [
                    {"id": Uuid::new_v4(), "name":"label","label":"Label","field_type":"string","required":true,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{}}
                ],
                "relationships": []
            }
        ]
    })
}

async fn publish_m2m(ctx: &Ctx) {
    let (_, d) = call(
        &ctx.app,
        "POST",
        "/api/studio/drafts",
        &ctx.token,
        Some(json!({"name":"m2m"}).to_string()),
        None,
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
        .body(Body::from(m2m_model().to_string()))
        .unwrap();
    let _ = ctx.app.clone().oneshot(req).await.unwrap();
    let (st, res) = call(
        &ctx.app,
        "POST",
        &format!("/api/studio/drafts/{id}/publish"),
        &ctx.token,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "publish: {res}");
}

#[tokio::test]
async fn crud_works_on_entity_with_m2m_relationship() {
    let Some(ctx) = setup().await else {
        return;
    };
    publish_m2m(&ctx).await;

    // THE regression: create hoists FK columns for every relationship — the
    // m2m field has no column (it materialized a join table), so every create
    // on an m2m-bearing entity used to 500 ("column ... does not exist").
    let (st, rec) = call(
        &ctx.app,
        "POST",
        "/api/data/Customer",
        &ctx.token,
        Some(json!({"name":"Acme"}).to_string()),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "create on m2m entity: {rec}");
    let id = rec["id"].as_str().unwrap().to_string();

    // update / read / list all address only real columns
    let (st, upd) = call(
        &ctx.app,
        "PATCH",
        &format!("/api/data/Customer/{id}"),
        &ctx.token,
        Some(json!({"name":"Acmex"}).to_string()),
        Some(1),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "update on m2m entity: {upd}");
    let (st, got) = call(
        &ctx.app,
        "GET",
        &format!("/api/data/Customer/{id}"),
        &ctx.token,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(got["name"], "Acmex");
    let (st, list) = call(
        &ctx.app,
        "GET",
        "/api/data/Customer?filter=name:eq:Acmex",
        &ctx.token,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "list on m2m entity: {list}");
    assert_eq!(list["total"], 1);

    // Addressing the m2m field anywhere in a record body is a clean 422 —
    // links live in the join table, not on the record row.
    let (st, err) = call(
        &ctx.app,
        "POST",
        "/api/data/Customer",
        &ctx.token,
        Some(json!({"name":"Beta","tags": "00000000-0000-0000-0000-000000000000"}).to_string()),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "m2m in body: {err}");
    let (st, err) = call(
        &ctx.app,
        "PATCH",
        &format!("/api/data/Customer/{id}"),
        &ctx.token,
        Some(json!({"tags": "00000000-0000-0000-0000-000000000000"}).to_string()),
        Some(2),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "m2m in patch: {err}");

    // Filtering / sorting by the m2m field: 422 (there is no column), not 500.
    let (st, _) = call(
        &ctx.app,
        "GET",
        "/api/data/Customer?filter=tags:eq:00000000-0000-0000-0000-000000000000",
        &ctx.token,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "filter by m2m field");
    let (st, _) = call(
        &ctx.app,
        "GET",
        "/api/data/Customer?sort=tags",
        &ctx.token,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "sort by m2m field");

    // delete → restore also walks the relationship column list; must work.
    let (st, _) = call(
        &ctx.app,
        "DELETE",
        &format!("/api/data/Customer/{id}"),
        &ctx.token,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let n: i64 = sqlx::query_scalar(&format!(
        "SELECT count(*) FROM biz_archive.{} WHERE tenant_id = $1",
        // table name is dynamic; find it from meta
        sqlx::query_scalar::<_, String>(
            "SELECT table_name FROM meta.md_entity WHERE tenant_id = $1 AND name = 'Customer'"
        )
        .bind(ctx.tenant)
        .fetch_one(&ctx.pool)
        .await
        .unwrap()
    ))
    .bind(ctx.tenant)
    .fetch_one(&ctx.pool)
    .await
    .unwrap();
    assert_eq!(n, 1, "archived");
    let (st, res) = call(
        &ctx.app,
        "POST",
        &format!("/api/data/Customer/{id}/restore"),
        &ctx.token,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "restore on m2m entity: {res}");
    assert_eq!(res["version"], 3);
}
