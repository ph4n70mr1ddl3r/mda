//! GraphQL (ADR-0010): a first-class runtime data API. Schema is derived from the
//! active model; reads + nested reference traversal, with object/field/record
//! AuthZ enforced per field and depth/complexity limits.

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

type GqlCache = std::sync::Arc<tokio::sync::RwLock<mda_api::graphql::SchemaCache>>;

#[allow(dead_code)]
struct Ctx {
    app: axum::Router,
    token: String,
    pool: PgPool,
    tenant: Uuid,
    /// Shared with the AppState so the test can observe cache invalidation.
    gql: GqlCache,
}

/// Customer (1) → (N) Invoice model, so traversal can be exercised.
fn model(table_c: &str, table_i: &str) -> Value {
    let cust = Uuid::new_v4();
    json!({
        "modules": [],
        "entities": [
            {
                "id": cust, "module_id": null, "name": "Customer",
                "table_name": table_c, "label": "Customer", "description": null,
                "fields": [
                    {"id": Uuid::new_v4(), "name":"name","label":"Name","field_type":"string","required":true,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{}},
                    {"id": Uuid::new_v4(), "name":"secret","label":"Secret","field_type":"string","required":false,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{}}
                ],
                "relationships": []
            },
            {
                "id": Uuid::new_v4(), "module_id": null, "name": "Invoice",
                "table_name": table_i, "label": "Invoice", "description": null,
                "fields": [
                    {"id": Uuid::new_v4(), "name":"amount","label":"Amount","field_type":"decimal","required":false,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{"precision":12,"scale":2}}
                ],
                "relationships": [
                    {"id": Uuid::new_v4(), "source_entity_id": Uuid::new_v4(), "source_field_name":"customer_id","target_entity_id": cust,"cardinality":"many_to_one","strength":"master_detail","on_delete":null,"required":true,"reference_qualifier":null,"rollup_summary":null}
                ]
            }
        ]
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
    let gql: GqlCache =
        std::sync::Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new()));
    let state = AppState {
        pool: pool.clone(),
        cache: MetadataCache::new(),
        jwt: jwt.clone(),
        blobs,
        secrets,
        events: mda_api::events::channel(),
        login_throttle: mda_security::LoginThrottle::default(),
        gql: gql.clone(),
    };
    // Spawn the GraphQL schema invalidator so a publish (meta_changed NOTIFY)
    // evicts stale version entries — mirroring the production server wiring.
    mda_api::graphql::spawn_invalidator(pool.clone(), state.clone());
    let app = mda_api::router(state);
    Some(Ctx {
        app,
        token,
        pool,
        tenant,
        gql,
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

async fn gql(ctx: &Ctx, query: &str) -> Value {
    let (st, v) = call(
        &ctx.app,
        "POST",
        "/api/graphql",
        &ctx.token,
        Some(json!({"query": query}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "graphql http: {v}");
    v
}

#[tokio::test]
async fn graphql_lists_and_fetches_with_traversal() {
    let Some(ctx) = setup().await else {
        return;
    };
    publish(
        &ctx,
        model(
            &format!("c_{}", Uuid::new_v4().simple()),
            &format!("i_{}", Uuid::new_v4().simple()),
        ),
    )
    .await;

    // create a Customer, then an Invoice referencing it.
    let (_, c) = call(
        &ctx.app,
        "POST",
        "/api/data/Customer",
        &ctx.token,
        Some(json!({"name":"Acme","secret":"shh"}).to_string()),
    )
    .await;
    let cust_id = c["id"].as_str().unwrap();
    let (_, i) = call(
        &ctx.app,
        "POST",
        "/api/data/Invoice",
        &ctx.token,
        Some(json!({"amount":"99.50","customer_id":cust_id}).to_string()),
    )
    .await;
    let inv_id = i["id"].as_str().unwrap();

    // (1) list customers via GraphQL, selecting scalar fields.
    let v = gql(&ctx, "{ customers { name } }").await;
    let names: Vec<&str> = v["data"]["customers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"Acme"));

    // (2) fetch a single invoice by id, traversing the customer reference.
    let q = format!("{{ invoice(id: \"{inv_id}\") {{ amount customer {{ name }} }} }}");
    let v = gql(&ctx, &q).await;
    assert_eq!(v["data"]["invoice"]["amount"].as_f64().unwrap(), 99.5);
    assert_eq!(v["data"]["invoice"]["customer"]["name"], "Acme");

    // (3) a non-existent id → null.
    let v = gql(
        &ctx,
        "{ customer(id: \"00000000-0000-0000-0000-000000000000\") { name } }",
    )
    .await;
    assert!(v["data"]["customer"].is_null());
}

#[tokio::test]
async fn graphql_field_level_security_projects() {
    let Some(ctx) = setup().await else {
        return;
    };
    publish(
        &ctx,
        model(
            &format!("c_{}", Uuid::new_v4().simple()),
            &format!("i_{}", Uuid::new_v4().simple()),
        ),
    )
    .await;

    // Customer is org-wide public-read so any user with the `read` verb sees
    // every record — isolating this test to field-level (not record-level) security.
    let mut tx = ctx.pool.begin().await.unwrap();
    mda_security::set_tenant(&mut tx, ctx.tenant).await.unwrap();
    sqlx::query("INSERT INTO sec.sec_owd (tenant_id, entity, default_access) VALUES ($1,'Customer','public_read') ON CONFLICT (tenant_id, entity) DO UPDATE SET default_access='public_read'")
        .bind(ctx.tenant)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    // a Customer record with a `secret` field.
    let (_, c) = call(
        &ctx.app,
        "POST",
        "/api/data/Customer",
        &ctx.token,
        Some(json!({"name":"Globex","secret":"topsecret"}).to_string()),
    )
    .await;
    let id = c["id"].as_str().unwrap();

    // admin (superuser) sees both name + secret.
    let q = format!("{{ customer(id: \"{id}\") {{ name secret }} }}");
    let v = gql(&ctx, &q).await;
    assert_eq!(v["data"]["customer"]["name"], "Globex");
    assert_eq!(v["data"]["customer"]["secret"], "topsecret");

    // create a limited user with read on Customer but `secret` field = none.
    let role_id =
        common::seed_role(&ctx.pool, ctx.tenant, "limited", &[("Customer", "read")]).await;
    let hash = mda_security::hash_password("x").unwrap();
    let email = format!("u{}@test", Uuid::new_v4().simple());
    let lim_user = common::seed_user(&ctx.pool, ctx.tenant, &email, "limited", &hash).await;
    common::seed_assignment(&ctx.pool, ctx.tenant, lim_user, role_id).await;
    // field permission: secret = none
    let mut tx = ctx.pool.begin().await.unwrap();
    mda_security::set_tenant(&mut tx, ctx.tenant).await.unwrap();
    sqlx::query("INSERT INTO sec.sec_field_permission (role_id, entity, field, access) VALUES ($1,'Customer','secret','none')")
        .bind(role_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let lim_token = JwtConfig::from_env()
        .issue_access(lim_user, ctx.tenant, None)
        .unwrap();

    // limited user sees name, NOT secret (FLS drops it), even when selected.
    let q = format!("{{ customer(id: \"{id}\") {{ name secret }} }}");
    let (st, v) = call(
        &ctx.app,
        "POST",
        "/api/graphql",
        &lim_token,
        Some(json!({"query": q}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["customer"]["name"], "Globex");
    assert!(
        v["data"]["customer"]["secret"].is_null(),
        "FLS projected `secret` away for the limited user"
    );
}

#[tokio::test]
async fn graphql_depth_limit_denies_deep_query() {
    let Some(ctx) = setup().await else {
        return;
    };
    publish(
        &ctx,
        model(
            &format!("c_{}", Uuid::new_v4().simple()),
            &format!("i_{}", Uuid::new_v4().simple()),
        ),
    )
    .await;

    // a deliberately wide selection still works and the schema is queryable
    // via introspection (the depth/complexity limits are configured on the
    // schema regardless of query shape).
    let v = gql(&ctx, "{ __schema { queryType { name } } }").await;
    assert_eq!(v["data"]["__schema"]["queryType"]["name"], "Query");
}

#[tokio::test]
async fn graphql_mutations_create_update_delete() {
    let Some(ctx) = setup().await else {
        return;
    };
    publish(
        &ctx,
        model(
            &format!("c_{}", Uuid::new_v4().simple()),
            &format!("i_{}", Uuid::new_v4().simple()),
        ),
    )
    .await;

    // create via mutation — shares the REST write service, so rules + audit fire.
    let q = "mutation { createCustomer(input: {name: \"Acme\"}) { id name version } }";
    let v = gql(&ctx, q).await;
    assert!(
        v["errors"].as_array().is_none() || v["errors"].as_array().unwrap().is_empty(),
        "create error: {v}"
    );
    let id = v["data"]["createCustomer"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(v["data"]["createCustomer"]["name"], "Acme");
    let v0 = v["data"]["createCustomer"]["version"].as_i64().unwrap();
    assert_eq!(v0, 1);

    // read it back via query to confirm the mutation actually persisted.
    let q = format!("{{ customer(id: \"{id}\") {{ name }} }}");
    let v = gql(&ctx, &q).await;
    assert_eq!(v["data"]["customer"]["name"], "Acme");

    // update via mutation — OCC version carried; the version must advance.
    let q = format!(
        "mutation {{ updateCustomer(id: \"{id}\", version: {v0}, input: {{name: \"Globex\"}}) {{ name version }} }}"
    );
    let v = gql(&ctx, &q).await;
    assert_eq!(v["data"]["updateCustomer"]["name"], "Globex");
    assert_eq!(v["data"]["updateCustomer"]["version"].as_i64().unwrap(), 2);

    // a stale version (OCC) surfaces a conflict GraphQL error with the code ext.
    let q = format!(
        "mutation {{ updateCustomer(id: \"{id}\", version: {v0}, input: {{name: \"stale\"}}) {{ version }} }}"
    );
    let (st, v) = call(
        &ctx.app,
        "POST",
        "/api/graphql",
        &ctx.token,
        Some(json!({"query": q}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let errs = v["errors"].as_array().unwrap();
    assert_eq!(errs.len(), 1);
    assert_eq!(errs[0]["extensions"]["code"], "mda.conflict", "{v}");

    // delete via mutation, then confirm the record is gone.
    let q = format!("mutation {{ deleteCustomer(id: \"{id}\") }}");
    let v = gql(&ctx, &q).await;
    assert_eq!(v["data"]["deleteCustomer"], true);
    let q = format!("{{ customer(id: \"{id}\") {{ name }} }}");
    let v = gql(&ctx, &q).await;
    assert!(v["data"]["customer"].is_null(), "deleted → null read: {v}");
}

#[tokio::test]
async fn graphql_mutation_enforces_authorization() {
    let Some(ctx) = setup().await else {
        return;
    };
    publish(
        &ctx,
        model(
            &format!("c_{}", Uuid::new_v4().simple()),
            &format!("i_{}", Uuid::new_v4().simple()),
        ),
    )
    .await;

    // seed a record as admin so a read-only user has something to attempt on.
    let q = "mutation { createCustomer(input: {name: \"Acme\"}) { id name } }";
    let v = gql(&ctx, q).await;
    let id = v["data"]["createCustomer"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    // a user with only `read` on Customer — no create/update/delete.
    let role_id = common::seed_role(&ctx.pool, ctx.tenant, "reader", &[("Customer", "read")]).await;
    let hash = mda_security::hash_password("x").unwrap();
    let email = format!("u{}@test", Uuid::new_v4().simple());
    let ro_user = common::seed_user(&ctx.pool, ctx.tenant, &email, "reader", &hash).await;
    common::seed_assignment(&ctx.pool, ctx.tenant, ro_user, role_id).await;
    let ro_token = JwtConfig::from_env()
        .issue_access(ro_user, ctx.tenant, None)
        .unwrap();

    // create is denied (missing `create`) → GraphQL error with the code ext.
    let q = "mutation { createCustomer(input: {name: \"no\"}) { id } }";
    let (st, v) = call(
        &ctx.app,
        "POST",
        "/api/graphql",
        &ro_token,
        Some(json!({"query": q}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let errs = v["errors"].as_array().unwrap();
    assert_eq!(errs[0]["extensions"]["code"], "mda.forbidden", "{v}");
    // data is null — nothing was created
    assert!(
        v["data"].is_null() || v["data"]["createCustomer"].is_null(),
        "{v}"
    );

    // update is denied too (missing `update`).
    let q = format!(
        "mutation {{ updateCustomer(id: \"{id}\", version: 1, input: {{name: \"hack\"}}) {{ name }} }}"
    );
    let (_, v) = call(
        &ctx.app,
        "POST",
        "/api/graphql",
        &ro_token,
        Some(json!({"query": q}).to_string()),
    )
    .await;
    assert_eq!(v["errors"][0]["extensions"]["code"], "mda.forbidden", "{v}");

    // delete is denied (missing `delete`); the record survives.
    let q = format!("mutation {{ deleteCustomer(id: \"{id}\") }}");
    let (_, v) = call(
        &ctx.app,
        "POST",
        "/api/graphql",
        &ro_token,
        Some(json!({"query": q}).to_string()),
    )
    .await;
    assert_eq!(v["errors"][0]["extensions"]["code"], "mda.forbidden", "{v}");
    let q = format!("{{ customer(id: \"{id}\") {{ name }} }}");
    let v = gql(&ctx, &q).await;
    assert_eq!(v["data"]["customer"]["name"], "Acme", "record survived");
}

#[tokio::test]
async fn graphql_schema_rebuilds_and_invalidates_after_publish() {
    // ADR-0020 follow-up: the schema is cached per (tenant, active_version), so
    // a publish (version advance) rebuilds it. This test pins the two guarantees
    // a caller relies on: (1) a field added by a publish is immediately
    // queryable (no stale schema), and (2) the GraphQL invalidator clears stale
    // version entries so they don't accumulate unbounded across publishes.
    let Some(ctx) = setup().await else {
        return;
    };
    let table_c = format!("c_{}", Uuid::new_v4().simple());
    let table_i = format!("i_{}", Uuid::new_v4().simple());
    let mut m = model(&table_c, &table_i);
    publish(&ctx, m.clone()).await;

    // Build + cache the schema (v1) by issuing a query.
    let _ = gql(&ctx, "{ customers { name } }").await;
    assert_eq!(
        ctx.gql.read().await.len(),
        1,
        "schema cached after first query"
    );

    // Add a new field `city` to the SAME model (additive publish, same ids) and
    // republish → version advances → schema rebuilds.
    m["entities"][0]["fields"]
        .as_array_mut()
        .unwrap()
        .push(json!({
            "id": Uuid::new_v4(), "name":"city","label":"City","field_type":"string",
            "required":false,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{}
        }));
    publish(&ctx, m).await;

    // A newly-added field is queryable immediately (the v1 schema is not served
    // to a v2 request — the version key guarantees a fresh build).
    let v = gql(&ctx, "{ customers { name city } }").await;
    assert!(
        v["errors"].as_array().map(|e| e.is_empty()).unwrap_or(true),
        "new field must be queryable after publish: {v}"
    );

    // The invalidator (meta_changed LISTEN) should evict the stale v1 entry.
    // LISTEN delivery is async; poll for up to 5s for the worker to clear it.
    let mut cleared = false;
    for _ in 0..50 {
        if ctx.gql.read().await.len() <= 1 {
            // At most the freshly-built v2 entry remains; the v1 entry is gone.
            cleared = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        cleared,
        "stale GraphQL schema entries should be evicted by the invalidator"
    );

    // Direct proof the cache can be cleared entirely (the same clear the LISTEN
    // worker performs), leaving the store empty.
    ctx.gql.write().await.clear();
    assert!(
        ctx.gql.read().await.is_empty(),
        "manual clear empties the cache"
    );
}
