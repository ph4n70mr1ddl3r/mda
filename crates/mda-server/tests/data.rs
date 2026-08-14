//! Phase 2/3 end-to-end: biz DDL + CRUD + OCC + native FK + migration + retire
//! (Phase 2), AND auth/RBAC/FLS/record-level + audit (Phase 3). Authenticated
//! via per-test superuser; a limited user exercises denials.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use mda_api::AppState;
use mda_meta::MetadataCache;
use mda_security::jwt::JwtConfig;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

mod common;

struct Ctx {
    app: axum::Router,
    token: String,
    jwt: JwtConfig,
    pool: PgPool,
    /// The non-superuser pool the app actually serves through (RLS active).
    app_pool: PgPool,
    tenant: Uuid,
    user_id: Uuid,
}

fn customer_model() -> Value {
    json!({
        "modules": [],
        "entities": [{
            "id": Uuid::new_v4(),
            "module_id": null,
            "name": "Customer",
            "table_name": format!("customer_{}", Uuid::new_v4().simple()),
            "label": "Customer",
            "description": null,
            "fields": [
                {"id": Uuid::new_v4(), "name":"name","label":"Name","field_type":"string","required":true,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{}},
                {"id": Uuid::new_v4(), "name":"email","label":"Email","field_type":"string","required":false,"is_unique":true,"is_indexed":false,"default_expr":null,"config":{}},
                {"id": Uuid::new_v4(), "name":"tier","label":"Tier","field_type":"enum","required":false,"is_unique":false,"is_indexed":true,"default_expr":null,"config":{"options":["Bronze","Silver","Gold"]}},
                {"id": Uuid::new_v4(), "name":"balance","label":"Balance","field_type":"decimal","required":false,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{"precision":12,"scale":2}}
            ],
            "relationships": []
        }]
    })
}

/// Build a DATABASE_URL that connects as the non-superuser `mda_app` role
/// (created by the RLS migration) by swapping the userinfo of `url`.
fn app_role_url(url: &str) -> String {
    if let Some(scheme_end) = url.find("://") {
        let rest = &url[scheme_end + 3..];
        if let Some(at) = rest.find('@') {
            return format!("{}://mda_app:mda@{}", &url[..scheme_end], &rest[at + 1..]);
        }
    }
    url.to_string()
}

async fn setup() -> Option<Ctx> {
    let url = match std::env::var("DATABASE_URL") {
        Ok(u) => u,
        Err(_) => {
            eprintln!("skipping: DATABASE_URL not set");
            return None;
        }
    };
    // Each test gets its own fresh, migrated database → fully parallel-safe.
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
    // The app runs as the non-superuser `mda_app` role so the biz.* RLS policies
    // actually engage (superusers BYPASS RLS). Migrations ran above as the
    // privileged owner; ctx.pool (superuser) is kept for direct assertions.
    let app_pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&app_role_url(&db_url))
        .await
        .unwrap_or_else(|e| {
            eprintln!(
                "RLS not exercised: could not connect as mda_app ({e}); falling back to owner pool"
            );
            pool.clone()
        });
    let app = mda_api::router(AppState {
        pool: app_pool.clone(),
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
        jwt,
        pool,
        app_pool,
        tenant,
        user_id,
    })
}

/// Create a user with the given permissions; return (token, user_id).
async fn limited_user(ctx: &Ctx, perms: &[(&str, &str)]) -> (String, Uuid) {
    let role_id = common::seed_role(
        &ctx.pool,
        ctx.tenant,
        &format!("r{}", Uuid::new_v4().simple()),
        perms,
    )
    .await;
    let hash = mda_security::hash_password("x").unwrap();
    let email = format!("u{}@test", Uuid::new_v4().simple());
    let user_id = common::seed_user(&ctx.pool, ctx.tenant, &email, "limited", &hash).await;
    common::seed_assignment(&ctx.pool, ctx.tenant, user_id, role_id).await;
    (
        ctx.jwt.issue_access(user_id, ctx.tenant, None).unwrap(),
        user_id,
    )
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    token: &str,
    body: Option<String>,
    if_match: Option<i64>,
) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(uri);
    b = b.header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"));
    if let Some(v) = if_match {
        b = b.header("if-match", v.to_string());
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

/// POST/GET with an explicit Content-Type + raw (non-JSON) body — used for the
/// CSV import path. Returns the parsed-JSON response envelope.
async fn call_with_type(
    app: &axum::Router,
    method: &str,
    uri: &str,
    token: &str,
    content_type: &str,
    body: String,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
        .header("content-type", content_type)
        .body(Body::from(body))
        .unwrap();
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

/// GET a text body (e.g. the CSV export) as a raw String.
async fn call_text(
    app: &axum::Router,
    method: &str,
    uri: &str,
    token: &str,
) -> (StatusCode, String) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

async fn publish(ctx: &Ctx, model: Value) -> Value {
    let (_, d) = call(
        &ctx.app,
        "POST",
        "/api/studio/drafts",
        &ctx.token,
        Some(json!({"name":"p"}).to_string()),
        None,
    )
    .await;
    let id = d["id"].as_str().unwrap().parse::<Uuid>().unwrap();
    let etag = d["version_etag"].as_str().unwrap().to_string();
    publish_with_uuid_etag(ctx, id, &etag, model).await
}

// The draft If-Match is a UUID etag (studio), not the data i64 version.
async fn publish_with_uuid_etag(ctx: &Ctx, draft_id: Uuid, etag: &str, model: Value) -> Value {
    let req = Request::builder()
        .method("PUT")
        .uri(format!("/api/studio/drafts/{draft_id}/model"))
        .header(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {}", ctx.token),
        )
        .header("if-match", etag)
        .header("content-type", "application/json")
        .body(Body::from(model.to_string()))
        .unwrap();
    let _ = ctx.app.clone().oneshot(req).await.unwrap();
    let (_, res) = call(
        &ctx.app,
        "POST",
        &format!("/api/studio/drafts/{draft_id}/publish"),
        &ctx.token,
        None,
        None,
    )
    .await;
    res
}

#[tokio::test]
async fn publish_creates_biz_table_and_crud_works() {
    let ctx = match setup().await {
        Some(c) => c,
        None => return,
    };
    publish(&ctx, customer_model()).await;

    let (st, rec) = call(
        &ctx.app,
        "POST",
        "/api/data/Customer",
        &ctx.token,
        Some(
            json!({"name":"Acme","email":"acme@example.com","tier":"Gold","balance":1234.5})
                .to_string(),
        ),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "create: {rec}");
    assert_eq!(rec["version"], 1);
    let id = rec["id"].as_str().unwrap().to_string();

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
    assert_eq!(got["email"], "acme@example.com");

    let (st, _) = call(
        &ctx.app,
        "PATCH",
        &format!("/api/data/Customer/{id}"),
        &ctx.token,
        Some(json!({"tier":"Silver"}).to_string()),
        Some(99),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
    let (st, upd) = call(
        &ctx.app,
        "PATCH",
        &format!("/api/data/Customer/{id}"),
        &ctx.token,
        Some(json!({"tier":"Silver"}).to_string()),
        Some(1),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "update: {upd}");
    assert_eq!(upd["version"], 2);

    let (st, list) = call(
        &ctx.app,
        "GET",
        "/api/data/Customer?filter=name:eq:Acme",
        &ctx.token,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "list: {list}");
    assert_eq!(list["total"], 1);

    // audit row was written
    let audits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sys_audit_log WHERE tenant_id = $1 AND entity = 'Customer'",
    )
    .bind(ctx.tenant)
    .fetch_one(&ctx.pool)
    .await
    .unwrap();
    assert!(
        audits >= 2,
        "expected audit rows (create+update), got {audits}"
    );

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
}

#[tokio::test]
async fn native_fk_enforced_for_references() {
    let ctx = match setup().await {
        Some(c) => c,
        None => return,
    };
    publish(&ctx, customer_model()).await;
    let (_, active) = call(&ctx.app, "GET", "/api/studio/model", &ctx.token, None, None).await;
    let customer_id = active["entities"][0]["id"]
        .as_str()
        .unwrap()
        .parse::<Uuid>()
        .unwrap();

    let mut model = active.clone();
    model["entities"].as_array_mut().unwrap().push(json!({
        "id": Uuid::new_v4(), "module_id": null, "name": "Order",
        "table_name": format!("order_{}", Uuid::new_v4().simple()), "label": "Order", "description": null,
        "fields": [{"id": Uuid::new_v4(), "name":"amount","label":"Amount","field_type":"decimal","required":true,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{"precision":12,"scale":2}}],
        "relationships": [{"id": Uuid::new_v4(), "source_field_name":"ref_customer_id","target_entity_id": customer_id,"cardinality":"many_to_one","strength":"lookup","on_delete":"set_null","required":false,"reference_qualifier":null,"rollup_summary":null}]
    }));
    publish(&ctx, model).await;

    let _ = call(
        &ctx.app,
        "POST",
        "/api/data/Customer",
        &ctx.token,
        Some(json!({"name":"Foo"}).to_string()),
        None,
    )
    .await;
    let (_, cust) = call(
        &ctx.app,
        "GET",
        "/api/data/Customer?filter=name:eq:Foo",
        &ctx.token,
        None,
        None,
    )
    .await;
    let cid = cust["items"][0]["id"].as_str().unwrap().to_string();

    let (st, _) = call(
        &ctx.app,
        "POST",
        "/api/data/Order",
        &ctx.token,
        Some(json!({"amount":10.0,"ref_customer_id":cid}).to_string()),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let (st, _) = call(
        &ctx.app,
        "POST",
        "/api/data/Order",
        &ctx.token,
        Some(json!({"amount":1.0,"ref_customer_id": Uuid::new_v4().to_string()}).to_string()),
        None,
    )
    .await;
    assert!(!st.is_success(), "dangling FK must be rejected (got {st})");
}

#[tokio::test]
async fn add_field_migration_and_retire() {
    let ctx = match setup().await {
        Some(c) => c,
        None => return,
    };
    let mut m = customer_model();
    m["entities"][0]["fields"] = json!([
        {"id": Uuid::new_v4(), "name":"name","label":"Name","field_type":"string","required":true,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{}},
        {"id": Uuid::new_v4(), "name":"email","label":"Email","field_type":"string","required":false,"is_unique":true,"is_indexed":false,"default_expr":null,"config":{}}
    ]);
    publish(&ctx, m.clone()).await;

    let mut with_phone = m.clone();
    with_phone["entities"][0]["fields"].as_array_mut().unwrap().push(json!({"id": Uuid::new_v4(), "name":"phone","label":"Phone","field_type":"string","required":false,"is_unique":false,"is_indexed":true,"default_expr":null,"config":{}}));
    let res = publish(&ctx, with_phone).await;
    assert_eq!(res["additions"]["fields"], 1, "add-field: {res}");

    let (st, rec) = call(
        &ctx.app,
        "POST",
        "/api/data/Customer",
        &ctx.token,
        Some(json!({"name":"Bar","email":"bar@example.com","phone":"555"}).to_string()),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{rec}");
    assert_eq!(rec["phone"], "555");

    // retire Customer (empty model -> two-phase retire)
    publish(&ctx, json!({"modules":[],"entities":[]})).await;
    let (st, _) = call(
        &ctx.app,
        "GET",
        "/api/data/Customer",
        &ctx.token,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn auth_rbac_and_record_level_enforced() {
    let ctx = match setup().await {
        Some(c) => c,
        None => return,
    };
    publish(&ctx, customer_model()).await;

    // no token -> 401
    let (st, _) = call(
        &ctx.app,
        "GET",
        "/api/data/Customer",
        "__none__",
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);

    // a user with NO permissions -> 403 on create (RBAC)
    let (none_token, _) = limited_user(&ctx, &[]).await;
    let (st, _) = call(
        &ctx.app,
        "POST",
        "/api/data/Customer",
        &none_token,
        Some(json!({"name":"X"}).to_string()),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "RBAC should deny create");

    // a user with read+create but no delete -> 403 on delete (RBAC)
    let (rw_token, _) = limited_user(&ctx, &[("Customer", "read"), ("Customer", "create")]).await;
    let (st, rec) = call(
        &ctx.app,
        "POST",
        "/api/data/Customer",
        &rw_token,
        Some(json!({"name":"OwnedByRW"}).to_string()),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "rw user create: {rec}");
    let id = rec["id"].as_str().unwrap().to_string();
    // record-level: rw_token OWNS this record; the admin (ctx.token) has OWD private default
    // -> admin cannot see/delete it unless OWD allows. Superuser bypasses, so use rw_token's
    // own view + the none_token cross-check instead.
    let (st, _) = call(
        &ctx.app,
        "DELETE",
        &format!("/api/data/Customer/{id}"),
        &rw_token,
        None,
        None,
    )
    .await;
    assert_eq!(
        st,
        StatusCode::FORBIDDEN,
        "RBAC should deny delete to rw user"
    );

    // record-level ownership: another reader (read perm) cannot see rw_token's private record
    let (reader_token, _) = limited_user(&ctx, &[("Customer", "read")]).await;
    let (st, got) = call(
        &ctx.app,
        "GET",
        &format!("/api/data/Customer/{id}"),
        &reader_token,
        None,
        None,
    )
    .await;
    assert_eq!(
        st,
        StatusCode::NOT_FOUND,
        "private record invisible to non-owner: {got}"
    );
}

#[tokio::test]
async fn rules_and_calculated_fields_fire() {
    let ctx = match setup().await {
        Some(c) => c,
        None => return,
    };
    // Ticket: qty, price, total(formula = qty*price), status, closed_at
    let model = json!({
        "modules": [],
        "entities": [{
            "id": Uuid::new_v4(), "module_id": null,
            "name": "Ticket", "table_name": format!("ticket_{}", Uuid::new_v4().simple()),
            "label": "Ticket", "description": null,
            "fields": [
                {"id": Uuid::new_v4(), "name":"status","label":"Status","field_type":"string","required":true,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{}},
                {"id": Uuid::new_v4(), "name":"closed_at","label":"Closed At","field_type":"datetime","required":false,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{}},
                {"id": Uuid::new_v4(), "name":"qty","label":"Qty","field_type":"integer","required":true,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{}},
                {"id": Uuid::new_v4(), "name":"price","label":"Price","field_type":"decimal","required":false,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{"precision":12,"scale":2}},
                {"id": Uuid::new_v4(), "name":"total","label":"Total","field_type":"decimal","required":false,"is_unique":false,"is_indexed":false,"default_expr":null,
                 "config":{"precision":12,"scale":2,"formula":{"op":"Arith","kind":"mul","lhs":{"op":"Field","name":"qty"},"rhs":{"op":"Field","name":"price"}}}}
            ],
            "relationships": []
        }]
    });
    publish(&ctx, model).await;

    // create -> calculated field total = qty*price
    let (st, rec) = call(
        &ctx.app,
        "POST",
        "/api/data/Ticket",
        &ctx.token,
        Some(json!({"status":"Open","qty":2,"price":10.0}).to_string()),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{rec}");
    assert_eq!(
        rec["total"].as_f64().unwrap(),
        20.0,
        "calculated field: {rec}"
    );
    let id = rec["id"].as_str().unwrap().to_string();

    // install a rule: when status becomes Closed, set closed_at = now()
    {
        let mut tx = ctx.pool.begin().await.unwrap();
        mda_security::set_tenant(&mut tx, ctx.tenant).await.unwrap();
        sqlx::query(
            "INSERT INTO meta.md_rule (tenant_id, entity, event, condition, action_type, action_field, action_value)
             VALUES ($1,'Ticket','after_update',
                '{\"op\":\"Cmp\",\"kind\":\"eq\",\"lhs\":{\"op\":\"Field\",\"name\":\"status\"},\"rhs\":{\"op\":\"Lit\",\"value\":\"Closed\"}}'::jsonb,
                'set_field','closed_at','{\"op\":\"Call\",\"name\":\"now\",\"args\":[]}'::jsonb)",
        )
        .bind(ctx.tenant)
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
    }

    // update status -> Closed => rule fires, closed_at set
    let (st, upd) = call(
        &ctx.app,
        "PATCH",
        &format!("/api/data/Ticket/{id}"),
        &ctx.token,
        Some(json!({"status":"Closed"}).to_string()),
        Some(1),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{upd}");
    assert!(
        upd["closed_at"].as_str().is_some_and(|s| !s.is_empty()),
        "closed_at should be set: {upd}"
    );
}

#[tokio::test]
async fn workflow_state_machine_runs() {
    let ctx = match setup().await {
        Some(c) => c,
        None => return,
    };
    let model = json!({
        "modules": [],
        "entities": [{
            "id": Uuid::new_v4(), "module_id": null,
            "name": "Invoice", "table_name": format!("inv_{}", Uuid::new_v4().simple()),
            "label": "Invoice", "description": null,
            "fields": [
                {"id": Uuid::new_v4(), "name":"amount","label":"Amount","field_type":"decimal","required":true,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{"precision":12,"scale":2}},
                {"id": Uuid::new_v4(), "name":"approved_at","label":"Approved At","field_type":"datetime","required":false,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{}}
            ],
            "relationships": []
        }]
    });
    publish(&ctx, model).await;

    // author the workflow via metadata (Studio is Phase 8). All md_workflow_* are
    // RLS-gated, so seed them under the tenant GUC in one txn (workflow_state /
    // transition get tenant_id via trigger from the workflow).
    let mut tx = ctx.pool.begin().await.unwrap();
    mda_security::set_tenant(&mut tx, ctx.tenant).await.unwrap();
    let (wf_id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO meta.md_workflow (tenant_id, entity, name) VALUES ($1,'Invoice','approval') RETURNING id",
    )
    .bind(ctx.tenant)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    for s in ["active", "Submitted", "Approved"] {
        sqlx::query("INSERT INTO meta.md_workflow_state (workflow_id, name) VALUES ($1,$2)")
            .bind(wf_id)
            .bind(s)
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO meta.md_workflow_transition (workflow_id, name, from_state, to_state, creates_task) VALUES ($1,'submit','active','Submitted',TRUE)")
        .bind(wf_id).execute(&mut *tx).await.unwrap();
    sqlx::query(
        "INSERT INTO meta.md_workflow_transition (workflow_id, name, from_state, to_state, guard, actions)
         VALUES ($1,'approve','Submitted','Approved',
            '{\"op\":\"Cmp\",\"kind\":\"gt\",\"lhs\":{\"op\":\"Field\",\"name\":\"amount\"},\"rhs\":{\"op\":\"Lit\",\"value\":0}}'::jsonb,
            '[{\"field\":\"approved_at\",\"value\":{\"op\":\"Call\",\"name\":\"now\",\"args\":[]}}]'::jsonb)",
    )
    .bind(wf_id).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();

    // create an Invoice (state defaults to 'active', version 1)
    let (_, rec) = call(
        &ctx.app,
        "POST",
        "/api/data/Invoice",
        &ctx.token,
        Some(json!({"amount":100.0}).to_string()),
        None,
    )
    .await;
    assert_eq!(rec["state"], "active");
    let id = rec["id"].as_str().unwrap().to_string();

    // submit -> Submitted, an approval task is created
    let (st, rec) = call(
        &ctx.app,
        "POST",
        &format!("/api/data/Invoice/{id}/submit"),
        &ctx.token,
        None,
        Some(1),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "submit: {rec}");
    assert_eq!(rec["state"], "Submitted");
    assert_eq!(rec["version"], 2);
    let tasks: i64 = {
        let mut tx = ctx.pool.begin().await.unwrap();
        mda_security::set_tenant(&mut tx, ctx.tenant).await.unwrap();
        sqlx::query_scalar(
            "SELECT count(*) FROM meta.md_workflow_task WHERE tenant_id=$1 AND record_id=$2",
        )
        .bind(ctx.tenant)
        .bind(Uuid::parse_str(&id).unwrap())
        .fetch_one(&mut *tx)
        .await
        .unwrap()
    };
    assert_eq!(tasks, 1, "approval task should be created");

    // approve -> Approved, approved_at set, outbox row written
    let (st, rec) = call(
        &ctx.app,
        "POST",
        &format!("/api/data/Invoice/{id}/approve"),
        &ctx.token,
        None,
        Some(2),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "approve: {rec}");
    assert_eq!(rec["state"], "Approved");
    assert!(rec["approved_at"].as_str().is_some_and(|s| !s.is_empty()));
    let outbox: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sys_outbox WHERE tenant_id=$1 AND kind='workflow.transitioned'",
    )
    .bind(ctx.tenant)
    .fetch_one(&ctx.pool)
    .await
    .unwrap();
    assert!(
        outbox >= 2,
        "expected outbox events for submit+approve, got {outbox}"
    );

    // guard rejection: a zero-amount invoice cannot be approved (guard amount>0)
    let (_, rec2) = call(
        &ctx.app,
        "POST",
        "/api/data/Invoice",
        &ctx.token,
        Some(json!({"amount":0.0}).to_string()),
        None,
    )
    .await;
    let id2 = rec2["id"].as_str().unwrap().to_string();
    call(
        &ctx.app,
        "POST",
        &format!("/api/data/Invoice/{id2}/submit"),
        &ctx.token,
        None,
        Some(1),
    )
    .await;
    let (st, _) = call(
        &ctx.app,
        "POST",
        &format!("/api/data/Invoice/{id2}/approve"),
        &ctx.token,
        None,
        Some(2),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::UNPROCESSABLE_ENTITY,
        "guard amount>0 should reject approve"
    );
}

#[tokio::test]
async fn report_runs_with_grouping_and_export() {
    let ctx = match setup().await {
        Some(c) => c,
        None => return,
    };
    // Customer with tier + amount
    let table = format!("cust_{}", Uuid::new_v4().simple());
    let model = json!({
        "modules": [],
        "entities": [{
            "id": Uuid::new_v4(), "module_id": null,
            "name": "Customer", "table_name": table,
            "label": "Customer", "description": null,
            "fields": [
                {"id": Uuid::new_v4(), "name":"tier","label":"Tier","field_type":"enum","required":false,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{"options":["Bronze","Silver","Gold"]}},
                {"id": Uuid::new_v4(), "name":"amount","label":"Amount","field_type":"decimal","required":false,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{"precision":12,"scale":2}}
            ],
            "relationships": []
        }]
    });
    publish(&ctx, model).await;

    // create rows: 2 Bronze (10, 20), 1 Silver (5)
    for (tier, amt) in [("Bronze", 10.0), ("Bronze", 20.0), ("Silver", 5.0)] {
        let _ = call(
            &ctx.app,
            "POST",
            "/api/data/Customer",
            &ctx.token,
            Some(json!({"tier":tier,"amount":amt}).to_string()),
            None,
        )
        .await;
    }

    // author a report: count + sum(amount) grouped by tier
    let dataset = json!({
        "base_entity":"Customer",
        "fields":[
            {"field":"tier"},
            {"field":"*","aggregate":"count","alias":"n"},
            {"field":"amount","aggregate":"sum","alias":"total"}
        ],
        "group_by":["tier"],
        "order_by":[{"field":"tier","asc":true}],
        "limit":10
    });
    let rep_id: Uuid = {
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
    };

    let (st, res) = call(
        &ctx.app,
        "GET",
        &format!("/api/reports/{rep_id}/run"),
        &ctx.token,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "run: {res}");
    let rows = res["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "two tiers: {res}");
    let by_tier: std::collections::HashMap<String, Value> = rows
        .iter()
        .map(|r| (r["tier"].as_str().unwrap().to_string(), r.clone()))
        .collect();
    assert_eq!(by_tier["Bronze"]["n"].as_i64().unwrap(), 2);
    assert_eq!(by_tier["Bronze"]["total"].as_f64().unwrap(), 30.0);
    assert_eq!(by_tier["Silver"]["n"].as_i64().unwrap(), 1);

    // CSV export (text/csv — assert status; the run above validated the data)
    let (st, _csv) = call(
        &ctx.app,
        "GET",
        &format!("/api/reports/{rep_id}/export"),
        &ctx.token,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "export status");
}

#[tokio::test]
async fn bulk_import_and_export() {
    let ctx = match setup().await {
        Some(c) => c,
        None => return,
    };
    let model = json!({
        "modules": [],
        "entities": [{
            "id": Uuid::new_v4(), "module_id": null,
            "name": "Customer", "table_name": format!("imp_{}", Uuid::new_v4().simple()),
            "label": "Customer", "description": null,
            "fields": [
                {"id": Uuid::new_v4(), "name":"name","label":"Name","field_type":"string","required":true,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{}},
                {"id": Uuid::new_v4(), "name":"email","label":"Email","field_type":"string","required":false,"is_unique":true,"is_indexed":false,"default_expr":null,"config":{}}
            ],
            "relationships": []
        }]
    });
    publish(&ctx, model).await;

    // import 3 rows: 2 valid, 1 missing required 'name'
    let rows = json!([
        {"name":"Alice","email":"a@x.com"},
        {"name":"Bob","email":"b@x.com"},
        {"email":"no-name@x.com"}
    ]);
    let (st, res) = call(
        &ctx.app,
        "POST",
        "/api/impex/Customer/import",
        &ctx.token,
        Some(rows.to_string()),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "import: {res}");
    assert_eq!(res["imported"], 2, "imported: {res}");
    assert_eq!(res["errors"].as_array().unwrap().len(), 1);

    // export as CSV (text/csv -> status check via JSON-parsing call)
    let (st, _csv) = call(
        &ctx.app,
        "GET",
        "/api/impex/Customer/export",
        &ctx.token,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "export status");
}

// ===== §5.13 impex: CSV import, dry-run, create/update/upsert, on_error =====

/// A small model with a unique key (email) for upsert/update tests.
fn keyed_model(table: &str) -> Value {
    json!({
        "modules": [],
        "entities": [{
            "id": Uuid::new_v4(), "module_id": null,
            "name": "Contact", "table_name": table,
            "label": "Contact", "description": null,
            "fields": [
                {"id": Uuid::new_v4(), "name":"name","label":"Name","field_type":"string","required":true,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{}},
                {"id": Uuid::new_v4(), "name":"email","label":"Email","field_type":"string","required":false,"is_unique":true,"is_indexed":true,"default_expr":null,"config":{}},
                {"id": Uuid::new_v4(), "name":"city","label":"City","field_type":"string","required":false,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{}}
            ],
            "relationships": []
        }]
    })
}

#[tokio::test]
async fn import_csv_creates_records() {
    let ctx = match setup().await {
        Some(c) => c,
        None => return,
    };
    publish(
        &ctx,
        keyed_model(&format!("imp_{}", Uuid::new_v4().simple())),
    )
    .await;

    // CSV with a quoted value containing a comma (round-trips through the parser).
    let csv =
        "name,email,city\nAda,ada@x.com,\"Cambridge, MA\"\nBob,bob@x.com,Oxford\n".to_string();
    let (st, res) = call_with_type(
        &ctx.app,
        "POST",
        "/api/impex/Contact/import",
        &ctx.token,
        "text/csv",
        csv,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "import: {res}");
    assert_eq!(res["imported"], 2, "imported: {res}");
    assert_eq!(res["created"], 2);
    assert_eq!(res["updated"], 0);
    assert_eq!(res["mode"], "create");
    assert_eq!(res["format"], "csv");
    assert!(
        res["errors"].as_array().unwrap().is_empty(),
        "errors: {res}"
    );

    // the quoted comma survived as data
    let (_st, res) = call(
        &ctx.app,
        "GET",
        "/api/data/Contact?filter=email:eq:ada@x.com",
        &ctx.token,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "read: {res}");
    assert_eq!(res["total"], 1);
    assert_eq!(res["items"][0]["city"], "Cambridge, MA");
}

#[tokio::test]
async fn import_dry_run_writes_nothing() {
    let ctx = match setup().await {
        Some(c) => c,
        None => return,
    };
    publish(
        &ctx,
        keyed_model(&format!("imp_{}", Uuid::new_v4().simple())),
    )
    .await;

    let csv = "name,email,city\nAda,ada@x.com,Cambridge\n,email@x.com,Bad\n".to_string();
    // dry_run: 1 valid (would_create), 1 invalid (missing required name); nothing written.
    let (st, res) = call_with_type(
        &ctx.app,
        "POST",
        "/api/impex/Contact/import?dry_run=true",
        &ctx.token,
        "text/csv",
        csv,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "dry_run: {res}");
    assert_eq!(res["dry_run"], true);
    assert_eq!(res["would_create"], 1);
    assert_eq!(res["created"], 0, "dry_run must not write");
    assert_eq!(res["updated"], 0);
    assert_eq!(res["errors"].as_array().unwrap().len(), 1);

    // nothing was actually written
    let (_st, res) = call(&ctx.app, "GET", "/api/data/Contact", &ctx.token, None, None).await;
    assert_eq!(res["total"], 0);
}

#[tokio::test]
async fn import_upsert_by_key_creates_and_updates() {
    let ctx = match setup().await {
        Some(c) => c,
        None => return,
    };
    publish(
        &ctx,
        keyed_model(&format!("imp_{}", Uuid::new_v4().simple())),
    )
    .await;

    // seed an existing record (owner = admin) to be matched by key=email
    let (st, _res) = call(
        &ctx.app,
        "POST",
        "/api/data/Contact",
        &ctx.token,
        Some(json!({"name":"Ada","email":"ada@x.com","city":"Old"}).to_string()),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);

    // upsert by key=email: Ada matches → update (city Old→New); Cyd is new → create
    let csv = "name,email,city\nAda,ada@x.com,New\nCyd,cyd@x.com,Paris\n".to_string();
    let (st, res) = call_with_type(
        &ctx.app,
        "POST",
        "/api/impex/Contact/import?mode=upsert&key=email",
        &ctx.token,
        "text/csv",
        csv,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "upsert: {res}");
    assert_eq!(res["created"], 1, "{res}");
    assert_eq!(res["updated"], 1, "{res}");
    assert_eq!(res["imported"], 2);
    assert!(
        res["errors"].as_array().unwrap().is_empty(),
        "errors: {res}"
    );

    // Ada's city was updated to New; total is now 2
    let (_st, res) = call(
        &ctx.app,
        "GET",
        "/api/data/Contact?filter=email:eq:ada@x.com",
        &ctx.token,
        None,
        None,
    )
    .await;
    assert_eq!(res["items"][0]["city"], "New");
    let (_st, res) = call(&ctx.app, "GET", "/api/data/Contact", &ctx.token, None, None).await;
    assert_eq!(res["total"], 2);
}

#[tokio::test]
async fn import_update_mode_missing_key_is_a_row_error() {
    let ctx = match setup().await {
        Some(c) => c,
        None => return,
    };
    publish(
        &ctx,
        keyed_model(&format!("imp_{}", Uuid::new_v4().simple())),
    )
    .await;

    // update by key=email, but no record has this email → row error, nothing written
    let csv = "name,email,city\nGhost,ghost@x.com,Nowhere\n".to_string();
    let (st, res) = call_with_type(
        &ctx.app,
        "POST",
        "/api/impex/Contact/import?mode=update&key=email",
        &ctx.token,
        "text/csv",
        csv,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "update: {res}");
    assert_eq!(res["updated"], 0);
    assert_eq!(res["errors"].as_array().unwrap().len(), 1);
    let (_st, res) = call(&ctx.app, "GET", "/api/data/Contact", &ctx.token, None, None).await;
    assert_eq!(res["total"], 0);
}

#[tokio::test]
async fn import_on_error_abort_writes_nothing() {
    let ctx = match setup().await {
        Some(c) => c,
        None => return,
    };
    publish(
        &ctx,
        keyed_model(&format!("imp_{}", Uuid::new_v4().simple())),
    )
    .await;

    // one good row, one bad (missing required name). on_error=abort ⇒ validate-then-
    // commit: the validation error means nothing is written.
    let csv = "name,email,city\nAda,ada@x.com,Cambridge\n,bad@x.com,Nope\n".to_string();
    let (st, res) = call_with_type(
        &ctx.app,
        "POST",
        "/api/impex/Contact/import?on_error=abort",
        &ctx.token,
        "text/csv",
        csv,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "abort: {res}");
    assert_eq!(res["on_error"], "abort");
    assert_eq!(res["imported"], 0, "abort must not partially commit: {res}");
    assert_eq!(res["would_create"], 1);
    assert_eq!(res["errors"].as_array().unwrap().len(), 1);

    let (_st, res) = call(&ctx.app, "GET", "/api/data/Contact", &ctx.token, None, None).await;
    assert_eq!(res["total"], 0);
}

#[tokio::test]
async fn import_rejects_unmapped_columns() {
    let ctx = match setup().await {
        Some(c) => c,
        None => return,
    };
    publish(
        &ctx,
        keyed_model(&format!("imp_{}", Uuid::new_v4().simple())),
    )
    .await;

    // 'bogus' is not a field → mapping error (422 invalid), not a per-row error.
    let csv = "name,email,bogus\nAda,ada@x.com,whatever\n".to_string();
    let (st, _res) = call_with_type(
        &ctx.app,
        "POST",
        "/api/impex/Contact/import",
        &ctx.token,
        "text/csv",
        csv,
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn import_export_round_trips() {
    let ctx = match setup().await {
        Some(c) => c,
        None => return,
    };
    publish(
        &ctx,
        keyed_model(&format!("imp_{}", Uuid::new_v4().simple())),
    )
    .await;

    // create two records
    for (n, e) in [("Ada", "ada@x.com"), ("Bob", "bob@x.com")] {
        call(
            &ctx.app,
            "POST",
            "/api/data/Contact",
            &ctx.token,
            Some(json!({"name":n,"email":e}).to_string()),
            None,
        )
        .await;
    }

    // export → CSV (id + readable fields)
    let (st, csv) = call_text(&ctx.app, "GET", "/api/impex/Contact/export", &ctx.token).await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        csv.contains("name") && csv.contains("email"),
        "export header: {csv}"
    );

    // re-import the exported CSV as upsert by id → every row matches (idempotent)
    let (st, res) = call_with_type(
        &ctx.app,
        "POST",
        "/api/impex/Contact/import?mode=upsert&key=id",
        &ctx.token,
        "text/csv",
        csv,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "round-trip: {res}");
    assert_eq!(
        res["updated"], 2,
        "all exported rows should match by id: {res}"
    );
    assert_eq!(res["created"], 0);
    assert!(
        res["errors"].as_array().unwrap().is_empty(),
        "errors: {res}"
    );

    // still exactly two records (no duplicates from the re-import)
    let (_st, res) = call(&ctx.app, "GET", "/api/data/Contact", &ctx.token, None, None).await;
    assert_eq!(res["total"], 2);
}

#[tokio::test]
async fn outbox_drains_into_notifications() {
    let ctx = match setup().await {
        Some(c) => c,
        None => return,
    };
    // enqueue a workflow.transitioned event addressed to the acting user
    sqlx::query(
        "INSERT INTO sys_outbox (tenant_id, kind, payload)
         VALUES ($1, 'workflow.transitioned', $2)",
    )
    .bind(ctx.tenant)
    .bind(serde_json::json!({
        "actor": ctx.user_id,
        "entity": "Invoice",
        "record_id": Uuid::new_v4(),
        "transition": "approve",
        "from": "Submitted",
        "to": "Approved",
    }))
    .execute(&ctx.pool)
    .await
    .unwrap();

    // start the drain worker and wait for it to process
    mda_server::outbox::spawn_drain(ctx.pool.clone());
    let mut done = false;
    for _ in 0..40 {
        let notified: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM sys_notification WHERE tenant_id=$1 AND user_id=$2",
        )
        .bind(ctx.tenant)
        .bind(ctx.user_id)
        .fetch_one(&ctx.pool)
        .await
        .unwrap();
        if notified > 0 {
            done = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    assert!(done, "drain worker should have created a notification");

    // outbox row marked done
    let pending: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sys_outbox WHERE tenant_id=$1 AND status='pending'",
    )
    .bind(ctx.tenant)
    .fetch_one(&ctx.pool)
    .await
    .unwrap();
    assert_eq!(pending, 0, "outbox should be drained");

    // the user sees it in their inbox
    let (st, list) = call(
        &ctx.app,
        "GET",
        "/api/notifications",
        &ctx.token,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "inbox: {list}");
    assert!(list
        .as_array()
        .unwrap()
        .iter()
        .any(|n| n["type"] == "workflow.transitioned"));
}

#[tokio::test]
async fn attachments_upload_download_and_field() {
    let ctx = match setup().await {
        Some(c) => c,
        None => return,
    };
    // upload (raw bytes + x-filename)
    let req = Request::builder()
        .method("POST")
        .uri("/api/attachments")
        .header(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {}", ctx.token),
        )
        .header("x-filename", "note.txt")
        .header("content-type", "text/plain")
        .body(Body::from("hello attachment"))
        .unwrap();
    let resp = ctx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let blob_id: Uuid = serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();

    // download (owner = actor)
    let (st, _) = call(
        &ctx.app,
        "GET",
        &format!("/api/attachments/{blob_id}"),
        &ctx.token,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "download");

    // an attachment field can store the blob id
    let model = json!({
        "modules": [],
        "entities": [{
            "id": Uuid::new_v4(), "module_id": null, "name": "Doc",
            "table_name": format!("doc_{}", Uuid::new_v4().simple()), "label": "Doc", "description": null,
            "fields": [{"id": Uuid::new_v4(), "name":"file","label":"File","field_type":"attachment","required":false,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{}}],
            "relationships": []
        }]
    });
    publish(&ctx, model).await;
    let (st, rec) = call(
        &ctx.app,
        "POST",
        "/api/data/Doc",
        &ctx.token,
        Some(json!({"file": blob_id.to_string()}).to_string()),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "create with attachment: {rec}");
    assert_eq!(rec["file"], blob_id.to_string());
}

#[tokio::test]
async fn record_sharing_makes_private_visible() {
    let ctx = match setup().await {
        Some(c) => c,
        None => return,
    };
    publish(&ctx, customer_model()).await;

    // admin creates a private record (OWD default = private)
    let (_, rec) = call(
        &ctx.app,
        "POST",
        "/api/data/Customer",
        &ctx.token,
        Some(json!({"name":"Secret"}).to_string()),
        None,
    )
    .await;
    let id = rec["id"].as_str().unwrap().to_string();

    // a reader (read perm) cannot see it (404)
    let (reader_token, reader_id) = limited_user(&ctx, &[("Customer", "read")]).await;
    let (st, _) = call(
        &ctx.app,
        "GET",
        &format!("/api/data/Customer/{id}"),
        &reader_token,
        None,
        None,
    )
    .await;
    assert_eq!(
        st,
        StatusCode::NOT_FOUND,
        "private record invisible before share"
    );
    let (_, list) = call(
        &ctx.app,
        "GET",
        "/api/data/Customer",
        &reader_token,
        None,
        None,
    )
    .await;
    assert_eq!(list["total"], 0, "list should be empty for non-owner");

    // admin shares with the reader
    let (st, _) = call(
        &ctx.app,
        "POST",
        &format!("/api/shares/Customer/{id}"),
        &ctx.token,
        Some(json!({"principal_id": reader_id, "access": "read"}).to_string()),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "share");

    // reader can now read + list it
    let (st, got) = call(
        &ctx.app,
        "GET",
        &format!("/api/data/Customer/{id}"),
        &reader_token,
        None,
        None,
    )
    .await;
    assert_eq!(
        st,
        StatusCode::OK,
        "shared record visible after share: {got}"
    );
    assert_eq!(got["name"], "Secret");
    let (_, list) = call(
        &ctx.app,
        "GET",
        "/api/data/Customer",
        &reader_token,
        None,
        None,
    )
    .await;
    assert_eq!(list["total"], 1, "shared record appears in list");
}

#[tokio::test]
async fn share_list_and_revoke() {
    let ctx = match setup().await {
        Some(c) => c,
        None => return,
    };
    publish(&ctx, customer_model()).await;

    // admin owns a private record
    let (_, rec) = call(
        &ctx.app,
        "POST",
        "/api/data/Customer",
        &ctx.token,
        Some(json!({"name":"Shared"}).to_string()),
        None,
    )
    .await;
    let id = rec["id"].as_str().unwrap().to_string();

    let (_reader_token, reader_id) = limited_user(&ctx, &[("Customer", "read")]).await;

    // initially no shares
    let (st, list) = call(
        &ctx.app,
        "GET",
        &format!("/api/shares/Customer/{id}"),
        &ctx.token,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "list shares: {list}");
    assert_eq!(list.as_array().unwrap().len(), 0, "no shares yet: {list}");

    // grant + list reflects it (with the principal's name/email)
    let (st, _) = call(
        &ctx.app,
        "POST",
        &format!("/api/shares/Customer/{id}"),
        &ctx.token,
        Some(json!({"principal_id": reader_id, "access": "read"}).to_string()),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "share");
    let (_st, list) = call(
        &ctx.app,
        "GET",
        &format!("/api/shares/Customer/{id}"),
        &ctx.token,
        None,
        None,
    )
    .await;
    assert_eq!(list.as_array().unwrap().len(), 1, "one share: {list}");
    assert_eq!(list[0]["principal_id"], json!(reader_id));
    assert_eq!(list[0]["access"], "read");
    assert!(
        list[0]["email"].as_str().unwrap().contains('@'),
        "name/email joined: {list}"
    );

    // re-grant upgrades access (still a single row)
    let (_st, _) = call(
        &ctx.app,
        "POST",
        &format!("/api/shares/Customer/{id}"),
        &ctx.token,
        Some(json!({"principal_id": reader_id, "access": "write"}).to_string()),
        None,
    )
    .await;
    let (_st, list) = call(
        &ctx.app,
        "GET",
        &format!("/api/shares/Customer/{id}"),
        &ctx.token,
        None,
        None,
    )
    .await;
    assert_eq!(
        list.as_array().unwrap().len(),
        1,
        "re-grant is an upsert: {list}"
    );
    assert_eq!(list[0]["access"], "write");

    // revoke + list is empty again
    let (st, _) = call(
        &ctx.app,
        "DELETE",
        &format!("/api/shares/Customer/{id}/{reader_id}"),
        &ctx.token,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT, "revoke");
    let (_st, list) = call(
        &ctx.app,
        "GET",
        &format!("/api/shares/Customer/{id}"),
        &ctx.token,
        None,
        None,
    )
    .await;
    assert_eq!(list.as_array().unwrap().len(), 0, "revoked: {list}");

    // revoking a non-existent share is 404
    let (st, _) = call(
        &ctx.app,
        "DELETE",
        &format!("/api/shares/Customer/{id}/{reader_id}"),
        &ctx.token,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND, "revoke missing");

    // a non-owner (the reader) may not list/revoke the admin's record's shares
    let (st, _) = call(
        &ctx.app,
        "GET",
        &format!("/api/shares/Customer/{id}"),
        &_reader_token,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "non-owner cannot list shares");
}

#[tokio::test]
async fn record_delete_archives_and_restore_recovers() {
    let ctx = match setup().await {
        Some(c) => c,
        None => return,
    };
    let table = format!("cust_{}", Uuid::new_v4().simple());
    publish(
        &ctx,
        json!({
            "modules": [],
            "entities": [{
                "id": Uuid::new_v4(), "module_id": null,
                "name": "Customer", "table_name": table, "label": "Customer", "description": null,
                "fields": [
                    {"id": Uuid::new_v4(),"name":"name","label":"Name","field_type":"string","required":true,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{}},
                    {"id": Uuid::new_v4(),"name":"email","label":"Email","field_type":"string","required":false,"is_unique":true,"is_indexed":false,"default_expr":null,"config":{}}
                ],
                "relationships": []
            }]
        }),
    )
    .await;

    let (st, rec) = call(
        &ctx.app,
        "POST",
        "/api/data/Customer",
        &ctx.token,
        Some(json!({"name":"Acme","email":"arc@x"}).to_string()),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "create: {rec}");
    let id = rec["id"].as_str().unwrap().to_string();

    // hard-delete → archive row appears (ADR-0006)
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

    // Count archives under this tenant's GUC (ctx.pool is non-superuser, so the
    // biz_archive RLS policy would otherwise hide the rows).
    let mut tx = ctx.pool.begin().await.unwrap();
    sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
        .bind(ctx.tenant.to_string())
        .execute(&mut *tx)
        .await
        .unwrap();
    let archived: i64 = sqlx::query_scalar(&format!(
        "SELECT count(*) FROM biz_archive.{table} WHERE id IS NOT NULL"
    ))
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    assert!(archived >= 1, "delete should have archived the row");

    // read → 404 (gone from live table)
    let (st, _) = call(
        &ctx.app,
        "GET",
        &format!("/api/data/Customer/{id}"),
        &ctx.token,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // restore → record is back, with a higher version (ADR-0015)
    let (st, restored) = call(
        &ctx.app,
        "POST",
        &format!("/api/data/Customer/{id}/restore"),
        &ctx.token,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "restore: {restored}");
    assert_eq!(restored["email"], "arc@x");
    assert_eq!(restored["version"], 2, "restored row gets a bumped version");

    // read now works again
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
    assert_eq!(got["email"], "arc@x");
}

#[tokio::test]
async fn write_path_emits_event_log() {
    let ctx = match setup().await {
        Some(c) => c,
        None => return,
    };
    publish(&ctx, customer_model()).await;

    let (_, rec) = call(
        &ctx.app,
        "POST",
        "/api/data/Customer",
        &ctx.token,
        Some(json!({"name":"Acme","email":"ev@x"}).to_string()),
        None,
    )
    .await;
    let id = rec["id"].as_str().unwrap().to_string();

    let created: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sys_event_log WHERE tenant_id=$1 AND entity='Customer' AND type='record.created'",
    )
    .bind(ctx.tenant)
    .fetch_one(&ctx.pool)
    .await
    .unwrap();
    assert_eq!(created, 1, "create should emit one record.created event");

    let _ = call(
        &ctx.app,
        "PATCH",
        &format!("/api/data/Customer/{id}"),
        &ctx.token,
        Some(json!({"tier":"Silver"}).to_string()),
        Some(1),
    )
    .await;
    let updated: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sys_event_log WHERE tenant_id=$1 AND entity='Customer' AND type='record.updated'",
    )
    .bind(ctx.tenant)
    .fetch_one(&ctx.pool)
    .await
    .unwrap();
    assert!(updated >= 1, "update should emit a record.updated event");

    let _ = call(
        &ctx.app,
        "DELETE",
        &format!("/api/data/Customer/{id}"),
        &ctx.token,
        None,
        None,
    )
    .await;
    let deleted: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sys_event_log WHERE tenant_id=$1 AND entity='Customer' AND type='record.deleted'",
    )
    .bind(ctx.tenant)
    .fetch_one(&ctx.pool)
    .await
    .unwrap();
    assert_eq!(deleted, 1, "delete should emit one record.deleted event");

    // events carry changed field *names* (never values) — verify shape.
    let (payload,): (Value,) = sqlx::query_as(
        "SELECT payload FROM sys_event_log WHERE tenant_id=$1 AND type='record.updated' ORDER BY seq DESC LIMIT 1",
    )
    .bind(ctx.tenant)
    .fetch_one(&ctx.pool)
    .await
    .unwrap();
    let changed = payload["changed_fields"].as_array().unwrap();
    assert!(changed.iter().any(|v| v == "tier"));
    assert!(
        !changed.iter().any(|v| v == "version" || v == "updated_at"),
        "internal versioning columns must not be reported as changed fields"
    );
}

#[tokio::test]
async fn sse_relay_replays_events_and_requires_auth() {
    let ctx = match setup().await {
        Some(c) => c,
        None => return,
    };
    publish(&ctx, customer_model()).await;
    let (_, rec) = call(
        &ctx.app,
        "POST",
        "/api/data/Customer",
        &ctx.token,
        Some(json!({"name":"Acme","email":"sse@x"}).to_string()),
        None,
    )
    .await;
    let _id = rec["id"].as_str().unwrap();

    // No token → 401.
    let req = Request::builder()
        .method("GET")
        .uri("/api/events")
        .body(Body::empty())
        .unwrap();
    let resp = ctx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    // With token + Last-Event-ID:0 → the replay (DB-backed, independent of the
    // live LISTEN worker) must deliver the committed record.created event.
    let req = Request::builder()
        .method("GET")
        .uri("/api/events")
        .header(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {}", ctx.token),
        )
        .header("Last-Event-ID", "0")
        .body(Body::empty())
        .unwrap();
    let resp = ctx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    use tokio_stream::StreamExt;
    let mut stream = resp.into_body().into_data_stream();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(2000);
    let mut got = String::new();
    while let Ok(Some(Ok(bytes))) = tokio::time::timeout_at(deadline, stream.next()).await {
        got.push_str(&String::from_utf8_lossy(&bytes));
        if got.contains("record.created") {
            break;
        }
    }
    assert!(
        got.contains("record.created"),
        "replay should deliver the committed event: {got}"
    );
    assert!(
        got.contains("id:"),
        "events carry a seq id (Last-Event-ID cursor)"
    );
}

#[tokio::test]
async fn edge_endpoints_security_headers_and_metrics() {
    let ctx = match setup().await {
        Some(c) => c,
        None => return,
    };

    // /livez is always 200 and does NOT touch the DB. (It returns plain text
    // "ok"; `call` JSON-parses, so only the status is asserted here.)
    let (st, _body) = call(&ctx.app, "GET", "/livez", &ctx.token, None, None).await;
    assert_eq!(st, StatusCode::OK, "livez");

    // /readyz reflects DB reachability.
    let (st, body) = call(&ctx.app, "GET", "/readyz", &ctx.token, None, None).await;
    assert_eq!(st, StatusCode::OK, "readyz: {body}");
    assert_eq!(body["database"], "up");

    // /metrics exposes the Prometheus text format with the expected series.
    let req = Request::builder()
        .method("GET")
        .uri("/metrics")
        .body(Body::empty())
        .unwrap();
    let resp = ctx.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("mda_http_requests_total"), "metrics: {text}");
    assert!(text.contains("mda_db_pool_size"), "metrics: {text}");
    assert!(
        text.contains("mda_audit_write_failures_total"),
        "metrics: {text}"
    );

    // Every response carries a request id + defense-in-depth security headers.
    let req = Request::builder()
        .method("GET")
        .uri("/health")
        .body(Body::empty())
        .unwrap();
    let resp = ctx.app.clone().oneshot(req).await.unwrap();
    assert!(
        resp.headers().get("x-request-id").is_some(),
        "request id echoed"
    );
    assert_eq!(
        resp.headers().get("x-content-type-options").unwrap(),
        "nosniff",
    );
    assert_eq!(resp.headers().get("x-frame-options").unwrap(), "DENY");
}

#[tokio::test]
async fn cross_tenant_data_is_isolated() {
    // Regression guard for tenant isolation (PLAN §5.4 / §11). The app filters
    // every biz.* query by tenant_id; a cross-tenant leak here means a future
    // change punched a hole. (RLS is the DB-layer backstop — enabled+forced at
    // publish in mda-data::ddl; exercised by `rls_gates_meta_at_db_layer`.)
    let ctx = match setup().await {
        Some(c) => c,
        None => return,
    };
    // Two tenants share ONE biz table (same table_name) — the hardest case.
    let table = format!("cust_{}", Uuid::new_v4().simple());
    // A factory so each tenant gets fresh ids but the SAME table_name (shared
    // biz table — the hardest cross-tenant case). md_* PKs are global.
    let model_for = |t: String| {
        json!({
            "modules": [],
            "entities": [{
                "id": Uuid::new_v4(), "module_id": null,
                "name": "Customer", "table_name": t, "label": "Customer", "description": null,
                "fields": [
                    {"id": Uuid::new_v4(),"name":"name","label":"Name","field_type":"string","required":true,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{}}
                ],
                "relationships": []
            }]
        })
    };
    let model = model_for(table.clone());
    publish(&ctx, model).await;

    // Tenant B: its own user/role/token in a DIFFERENT tenant.
    let tenant_b = Uuid::new_v4();
    let role_b = common::seed_role(&ctx.pool, tenant_b, "admin", &[("*", "*")]).await;
    let email_b = format!("b{}@test", Uuid::new_v4().simple());
    let user_b = common::seed_user(
        &ctx.pool,
        tenant_b,
        &email_b,
        "b",
        &mda_security::hash_password("x").unwrap(),
    )
    .await;
    common::seed_assignment(&ctx.pool, tenant_b, user_b, role_b).await;
    let token_b = ctx.jwt.issue_access(user_b, tenant_b, None).unwrap();

    // Publish the SAME model for tenant B (same table → shared biz.<table>).
    let (_, d) = call(
        &ctx.app,
        "POST",
        "/api/studio/drafts",
        &token_b,
        Some(json!({"name":"p"}).to_string()),
        None,
    )
    .await;
    let did = d["id"].as_str().unwrap().parse::<Uuid>().unwrap();
    let etag = d["version_etag"].as_str().unwrap().to_string();
    // Publish for tenant B as B (fresh ids, same table_name → shared biz table).
    let model_b = model_for(table.clone());
    let put = Request::builder()
        .method("PUT")
        .uri(format!("/api/studio/drafts/{did}/model"))
        .header(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {token_b}"),
        )
        .header("if-match", etag)
        .header("content-type", "application/json")
        .body(Body::from(model_b.to_string()))
        .unwrap();
    let _ = ctx.app.clone().oneshot(put).await.unwrap();
    let (pst, pr) = call(
        &ctx.app,
        "POST",
        &format!("/api/studio/drafts/{did}/publish"),
        &token_b,
        None,
        None,
    )
    .await;
    assert_eq!(pst, StatusCode::OK, "B publish: {pr}");

    // Tenant A creates a record.
    let (_, a_rec) = call(
        &ctx.app,
        "POST",
        "/api/data/Customer",
        &ctx.token,
        Some(json!({"name":"Acme-A"}).to_string()),
        None,
    )
    .await;
    let a_id = a_rec["id"].as_str().unwrap().to_string();

    // Tenant B lists → must NOT see tenant A's record (0 results).
    let (_, list_b) = call(&ctx.app, "GET", "/api/data/Customer", &token_b, None, None).await;
    assert_eq!(
        list_b["total"], 0,
        "tenant B must not see tenant A's records: {list_b}"
    );

    // Tenant B reads tenant A's record id by id → 404 (no leak, no 200).
    let (st, _) = call(
        &ctx.app,
        "GET",
        &format!("/api/data/Customer/{a_id}"),
        &token_b,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND, "cross-tenant read must 404");

    // Tenant B writes its own record into the shared table; tenant A must not see it.
    let _ = call(
        &ctx.app,
        "POST",
        "/api/data/Customer",
        &token_b,
        Some(json!({"name":"Beta-B"}).to_string()),
        None,
    )
    .await;
    let (_, list_a) = call(
        &ctx.app,
        "GET",
        "/api/data/Customer",
        &ctx.token,
        None,
        None,
    )
    .await;
    assert_eq!(list_a["total"], 1, "tenant A sees only its own: {list_a}");
}

#[tokio::test]
async fn rls_enforces_tenant_isolation_at_db_layer() {
    // Direct proof that the biz.* RLS policy engages at the POSTGRES layer —
    // independent of the app's tenant_id filters. Connects as the non-superuser
    // `mda_app` role and queries with no / wrong / correct tenant GUC.
    let ctx = match setup().await {
        Some(c) => c,
        None => return,
    };
    let table = format!("rls_probe_{}", Uuid::new_v4().simple());
    publish(
        &ctx,
        json!({
            "modules": [],
            "entities": [{
                "id": Uuid::new_v4(), "module_id": null,
                "name": "Probe", "table_name": table, "label": "Probe", "description": null,
                "fields": [
                    {"id": Uuid::new_v4(),"name":"k","label":"K","field_type":"string","required":true,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{}}
                ],
                "relationships": []
            }]
        }),
    )
    .await;
    let (_, _) = call(
        &ctx.app,
        "POST",
        "/api/data/Probe",
        &ctx.token,
        Some(json!({"k":"v"}).to_string()),
        None,
    )
    .await;

    // The app serves through ctx.app_pool as a NON-SUPERUSER role, so the biz.*
    // RLS policy engages (in docker that role is mda_app; in any non-superuser
    // deployment the connection role already qualifies). These probes bypass
    // the app's tenant_id filters entirely — they are a direct DB-layer check.

    // (1) No tenant GUC → RLS fails closed (0 rows). A query that forgets the
    //     GUC can never leak across tenants.
    let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM biz.{table}"))
        .fetch_one(&ctx.app_pool)
        .await
        .unwrap();
    assert_eq!(
        n, 0,
        "non-superuser with no GUC must see 0 rows (fail-closed)"
    );

    // (2) Correct tenant GUC → sees the row.
    let mut tx = ctx.app_pool.begin().await.unwrap();
    sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
        .bind(ctx.tenant.to_string())
        .execute(&mut *tx)
        .await
        .unwrap();
    let ok: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM biz.{table}"))
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(ok, 1, "with the right GUC the tenant's row is visible");
    tx.rollback().await.unwrap();

    // (3) Wrong tenant GUC → 0 rows.
    let mut tx = ctx.app_pool.begin().await.unwrap();
    sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
        .bind(Uuid::new_v4().to_string())
        .execute(&mut *tx)
        .await
        .unwrap();
    let wrong: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM biz.{table}"))
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(wrong, 0, "with a wrong GUC must see 0 rows");
}

#[tokio::test]
async fn tenant_guc_does_not_leak_across_pool_checkouts() {
    // Regression guard for a subtle RLS bypass: mda-data sets app.tenant_id
    // transaction-LOCAL. If it ever leaked to the session, a *later* query on a
    // reused pooled connection (e.g. a report, which does NOT set the GUC)
    // would see another tenant's rows. This test proves the GUC is scoped to the
    // create txn only.
    let ctx = match setup().await {
        Some(c) => c,
        None => return,
    };
    let table = format!("leak_{}", Uuid::new_v4().simple());
    publish(
        &ctx,
        json!({
            "modules": [],
            "entities": [{
                "id": Uuid::new_v4(), "module_id": null,
                "name": "Leak", "table_name": table, "label": "L", "description": null,
                "fields": [
                    {"id": Uuid::new_v4(),"name":"k","label":"K","field_type":"string","required":true,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{}}
                ],
                "relationships": []
            }]
        }),
    )
    .await;
    // Create via the app (mda_app) — this sets the GUC inside the create txn.
    let _ = call(
        &ctx.app,
        "POST",
        "/api/data/Leak",
        &ctx.token,
        Some(json!({"k":"v"}).to_string()),
        None,
    )
    .await;

    // Now, on the SAME app pool (a connection that may have served the create),
    // issue a raw biz query with NO tenant GUC set. RLS must block it (0 rows).
    // If this returns 1, the GUC leaked to the session and RLS is bypassable.
    let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM biz.{table}"))
        .fetch_one(&ctx.app_pool)
        .await
        .unwrap();
    assert_eq!(
        n, 0,
        "app.tenant_id leaked to the session — a later GUC-less query saw tenant data (RLS bypass)"
    );
}

#[tokio::test]
async fn rls_gates_sec_record_share_at_db_layer() {
    // sec_record_share (and sec_owd) carry tenant_id and are read/written only
    // in request context, so they are RLS-gated. This proves the policy engages
    // at the Postgres layer — a GUC-less or wrong-tenant query sees nothing.
    let ctx = match setup().await {
        Some(c) => c,
        None => return,
    };
    // Seed a share row for this tenant under the tenant GUC (RLS WITH CHECK
    // blocks a GUC-less insert — which itself confirms the policy is live).
    let mut tx = ctx.pool.begin().await.unwrap();
    mda_security::set_tenant(&mut tx, ctx.tenant).await.unwrap();
    sqlx::query(
        "INSERT INTO sec.sec_record_share (tenant_id, entity, record_id, principal_id, access)
         VALUES ($1, 'Customer', $2, $3, 'read')",
    )
    .bind(ctx.tenant)
    .bind(Uuid::new_v4())
    .bind(ctx.user_id)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();

    // (1) app role, NO tenant GUC → RLS fails closed (0 rows).
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM sec.sec_record_share")
        .fetch_one(&ctx.app_pool)
        .await
        .unwrap();
    assert_eq!(n, 0, "sec_record_share: no GUC must see 0 rows");

    // (2) correct tenant GUC → the row is visible.
    let mut tx = ctx.app_pool.begin().await.unwrap();
    mda_security::set_tenant(&mut tx, ctx.tenant).await.unwrap();
    let ok: i64 = sqlx::query_scalar("SELECT count(*) FROM sec.sec_record_share")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(ok, 1, "sec_record_share: right GUC sees the tenant's row");
    tx.rollback().await.unwrap();

    // (3) wrong tenant GUC → 0 rows.
    let mut tx = ctx.app_pool.begin().await.unwrap();
    mda_security::set_tenant(&mut tx, Uuid::new_v4())
        .await
        .unwrap();
    let wrong: i64 = sqlx::query_scalar("SELECT count(*) FROM sec.sec_record_share")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(wrong, 0, "sec_record_share: wrong GUC sees 0 rows");
}

#[tokio::test]
async fn tenant_scoped_login_and_sec_user_rls() {
    let ctx = match setup().await {
        Some(c) => c,
        None => return,
    };
    // A user with a known password in ctx.tenant (no role needed for login).
    let email = format!("login_{}@test", Uuid::new_v4().simple());
    let pass = "hunter2";
    let _uid = common::seed_user(
        &ctx.pool,
        ctx.tenant,
        &email,
        "loginer",
        &mda_security::hash_password(pass).unwrap(),
    )
    .await;

    // (1) Correct tenant (UUID) + password → tokens.
    let (st, body) = call(
        &ctx.app,
        "POST",
        "/api/auth/login",
        "",
        Some(json!({"tenant": ctx.tenant, "email": email, "password": pass}).to_string()),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "login: {body}");
    let token = body["access_token"].as_str().unwrap().to_string();
    // The issued JWT must carry the right tenant and work for /me (load_identity
    // under the JWT tenant → sec_user RLS allows the lookup).
    let (st, me) = call(&ctx.app, "GET", "/api/auth/me", &token, None, None).await;
    assert_eq!(st, StatusCode::OK, "me: {me}");
    assert_eq!(me["tenant_id"], json!(ctx.tenant));

    // (2) WRONG tenant + the same email → invalid credentials. sec_user is
    //     RLS-gated, so tenant B's GUC hides tenant A's user entirely.
    let (st, _) = call(
        &ctx.app,
        "POST",
        "/api/auth/login",
        "",
        Some(json!({"tenant": Uuid::new_v4(), "email": email, "password": pass}).to_string()),
        None,
    )
    .await;
    assert!(
        st == StatusCode::UNPROCESSABLE_ENTITY || st == StatusCode::UNAUTHORIZED,
        "wrong-tenant login must fail"
    );

    // (3) Slug-based login: register a slug for ctx.tenant (sec_tenant has no RLS).
    let slug = format!("acme-{}", Uuid::new_v4().simple());
    sqlx::query("INSERT INTO sec.sec_tenant (id, slug, name) VALUES ($1, $2, 'Acme') ON CONFLICT (id) DO UPDATE SET slug = $2, name = 'Acme'")
        .bind(ctx.tenant)
        .bind(&slug)
        .execute(&ctx.pool)
        .await
        .unwrap();
    let (st, body) = call(
        &ctx.app,
        "POST",
        "/api/auth/login",
        "",
        Some(json!({"tenant": slug, "email": email, "password": pass}).to_string()),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "slug login: {body}");

    // (4) sec_user RLS at the Postgres layer (via the non-superuser app pool):
    //     no GUC → 0; correct GUC → the user; wrong GUC → 0.
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM sec.sec_user")
        .fetch_one(&ctx.app_pool)
        .await
        .unwrap();
    assert_eq!(n, 0, "sec_user: no GUC must see 0 rows");
    let mut tx = ctx.app_pool.begin().await.unwrap();
    mda_security::set_tenant(&mut tx, ctx.tenant).await.unwrap();
    let ok: i64 = sqlx::query_scalar("SELECT count(*) FROM sec.sec_user WHERE email = $1")
        .bind(&email)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(ok, 1, "sec_user: right GUC sees the user");
    let mut tx = ctx.app_pool.begin().await.unwrap();
    mda_security::set_tenant(&mut tx, Uuid::new_v4())
        .await
        .unwrap();
    let wrong: i64 = sqlx::query_scalar("SELECT count(*) FROM sec.sec_user")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(wrong, 0, "sec_user: wrong GUC sees 0 rows");
}

#[tokio::test]
async fn rls_gates_meta_at_db_layer() {
    // meta.md_* is RLS-gated (except md_active_version, polled cross-tenant by
    // the cache worker). Prove it at the Postgres layer: a non-superuser with no
    // tenant GUC sees nothing; with the right GUC it sees its tenant's model;
    // with a wrong GUC, nothing.
    let ctx = match setup().await {
        Some(c) => c,
        None => return,
    };
    publish(
        &ctx,
        json!({"modules":[],"entities":[{
            "id": Uuid::new_v4(),"module_id":null,"name":"MetaProbe","table_name":format!("mp_{}", Uuid::new_v4().simple()),
            "label":"MP","description":null,
            "fields":[{"id": Uuid::new_v4(),"name":"k","label":"K","field_type":"string","required":true,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{}}],
            "relationships":[]
        }]}),
    )
    .await;

    // no GUC → 0 (a forgotten filter can never leak another tenant's model).
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM meta.md_entity")
        .fetch_one(&ctx.app_pool)
        .await
        .unwrap();
    assert_eq!(n, 0, "meta.md_entity: no GUC must see 0 rows");

    // correct tenant GUC → the entity is visible.
    let mut tx = ctx.app_pool.begin().await.unwrap();
    mda_security::set_tenant(&mut tx, ctx.tenant).await.unwrap();
    let ok: i64 =
        sqlx::query_scalar("SELECT count(*) FROM meta.md_entity WHERE name = 'MetaProbe'")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    assert_eq!(ok, 1, "meta.md_entity: right GUC sees the tenant's entity");
    tx.rollback().await.unwrap();

    // wrong tenant GUC → 0.
    let mut tx = ctx.app_pool.begin().await.unwrap();
    mda_security::set_tenant(&mut tx, Uuid::new_v4())
        .await
        .unwrap();
    let wrong: i64 = sqlx::query_scalar("SELECT count(*) FROM meta.md_entity")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(wrong, 0, "meta.md_entity: wrong GUC sees 0 rows");

    // md_active_version is INTENTIONALLY exempt (cache poller reads all tenants):
    // a GUC-less read must succeed (not be blocked).
    let _: i64 = sqlx::query_scalar("SELECT count(*) FROM meta.md_active_version")
        .fetch_one(&ctx.app_pool)
        .await
        .unwrap();
}

// ===== §14: record/field history + as-of (PLAN §14 surfaced capability) =====

#[tokio::test]
async fn record_history_timeline_and_field_diffs() {
    let ctx = match setup().await {
        Some(c) => c,
        None => return,
    };
    publish(&ctx, customer_model()).await;

    let (_, rec) = call(
        &ctx.app,
        "POST",
        "/api/data/Customer",
        &ctx.token,
        Some(json!({"name":"Acme","email":"h@x","tier":"Bronze"}).to_string()),
        None,
    )
    .await;
    let id = rec["id"].as_str().unwrap().to_string();
    // update tier + name → a second audit row with a per-field diff.
    let _ = call(
        &ctx.app,
        "PATCH",
        &format!("/api/data/Customer/{id}"),
        &ctx.token,
        Some(json!({"tier":"Gold","name":"Acme Co"}).to_string()),
        Some(1),
    )
    .await;

    let (st, body) = call(
        &ctx.app,
        "GET",
        &format!("/api/data/Customer/{id}/history"),
        &ctx.token,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "history: {body}");
    let entries = body["entries"].as_array().unwrap();
    assert!(entries.len() >= 2, "need create+update: {body}");
    // newest-first: first entry is the update.
    let upd = &entries[0];
    assert_eq!(upd["op"], "update");
    assert_eq!(upd["version"], 2);
    let changed: std::collections::HashSet<String> = upd["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["field"].as_str().unwrap().to_string())
        .collect();
    assert!(changed.contains("tier"), "tier changed: {upd}");
    assert!(changed.contains("name"), "name changed: {upd}");
    // internal columns never appear as changes.
    assert!(!changed.contains("version"));
    assert!(!changed.contains("updated_at"));
    // the diff carries from→to values for a readable field.
    let tier_change = upd["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["field"] == "tier")
        .unwrap();
    assert_eq!(tier_change["from"], "Bronze");
    assert_eq!(tier_change["to"], "Gold");
}

#[tokio::test]
async fn as_of_reconstructs_prior_versions() {
    let ctx = match setup().await {
        Some(c) => c,
        None => return,
    };
    publish(&ctx, customer_model()).await;
    let (_, rec) = call(
        &ctx.app,
        "POST",
        "/api/data/Customer",
        &ctx.token,
        Some(json!({"name":"Acme","tier":"Bronze"}).to_string()),
        None,
    )
    .await;
    let id = rec["id"].as_str().unwrap().to_string();
    let _ = call(
        &ctx.app,
        "PATCH",
        &format!("/api/data/Customer/{id}"),
        &ctx.token,
        Some(json!({"tier":"Gold"}).to_string()),
        Some(1),
    )
    .await;

    // as-of version 1 → the original tier.
    let (st, v1) = call(
        &ctx.app,
        "GET",
        &format!("/api/data/Customer/{id}/as-of?version=1"),
        &ctx.token,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "as-of v1: {v1}");
    assert_eq!(v1["tier"], "Bronze");
    assert_eq!(v1["version"], 1);

    // as-of version 2 → the updated tier.
    let (_, v2) = call(
        &ctx.app,
        "GET",
        &format!("/api/data/Customer/{id}/as-of?version=2"),
        &ctx.token,
        None,
        None,
    )
    .await;
    assert_eq!(v2["tier"], "Gold");

    // unknown version → 404 with the stable code.
    let (st, miss) = call(
        &ctx.app,
        "GET",
        &format!("/api/data/Customer/{id}/as-of?version=999"),
        &ctx.token,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(miss["code"], "mda.not_found", "error code in body: {miss}");

    // bad params → 422 with code.
    let (st, bad) = call(
        &ctx.app,
        "GET",
        &format!("/api/data/Customer/{id}/as-of"),
        &ctx.token,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(bad["code"], "mda.invalid");
}

#[tokio::test]
async fn history_respects_object_record_and_field_security() {
    let ctx = match setup().await {
        Some(c) => c,
        None => return,
    };
    publish(&ctx, customer_model()).await;
    let (_, rec) = call(
        &ctx.app,
        "POST",
        "/api/data/Customer",
        &ctx.token,
        Some(json!({"name":"Secret","tier":"Gold","email":"s@x"}).to_string()),
        None,
    )
    .await;
    let id = rec["id"].as_str().unwrap().to_string();

    // (1) no object read perm → 403 (code).
    let (none_token, _) = limited_user(&ctx, &[]).await;
    let (st, body) = call(
        &ctx.app,
        "GET",
        &format!("/api/data/Customer/{id}/history"),
        &none_token,
        None,
        None,
    )
    .await;
    assert_eq!(
        st,
        StatusCode::FORBIDDEN,
        "object-level denies history: {body}"
    );
    assert_eq!(body["code"], "mda.forbidden");

    // (2) a reader who can't see the private record (OWD private, not owner) → 404.
    let (reader_token, _) = limited_user(&ctx, &[("Customer", "read")]).await;
    let (st, _) = call(
        &ctx.app,
        "GET",
        &format!("/api/data/Customer/{id}/history"),
        &reader_token,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND, "record-level hides history");

    // (3) field-level: a reader with 'none' on 'tier' must not see tier in diffs.
    //     Share the record so the reader can read it, then restrict tier.
    let (fls_token, fls_user) = limited_user(&ctx, &[("Customer", "read")]).await;
    let _ = call(
        &ctx.app,
        "POST",
        &format!("/api/shares/Customer/{id}"),
        &ctx.token,
        Some(json!({"principal_id": fls_user, "access": "read"}).to_string()),
        None,
    )
    .await;
    // Seed a fresh role with read perm + an FLS 'none' on 'tier' and assign it
    // to the FLS user. sec_* tables are RLS-gated, so seed under the tenant GUC.
    {
        let mut tx = ctx.pool.begin().await.unwrap();
        mda_security::set_tenant(&mut tx, ctx.tenant).await.unwrap();
        let (role_id,): (Uuid,) = sqlx::query_as(
            "INSERT INTO sec.sec_role (tenant_id, name) VALUES ($1,$2) RETURNING id",
        )
        .bind(ctx.tenant)
        .bind(format!("fls_none_{}", Uuid::new_v4().simple()))
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO sec.sec_permission (role_id, entity, verb) VALUES ($1,'Customer','read')",
        )
        .bind(role_id)
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO sec.sec_field_permission (role_id, entity, field, access)
             VALUES ($1,'Customer','tier','none')",
        )
        .bind(role_id)
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query("INSERT INTO sec.sec_role_assignment (user_id, role_id) VALUES ($1,$2)")
            .bind(fls_user)
            .bind(role_id)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }
    let (st, body) = call(
        &ctx.app,
        "GET",
        &format!("/api/data/Customer/{id}/history"),
        &fls_token,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "shared reader history: {body}");
    // tier must be absent from every diff entry (FLS none).
    for entry in body["entries"].as_array().unwrap() {
        for ch in entry["changes"].as_array().unwrap() {
            assert_ne!(
                ch["field"], "tier",
                "FLS-none field leaked into history: {body}"
            );
        }
    }
}

#[tokio::test]
async fn history_of_deleted_record_is_admin_only() {
    let ctx = match setup().await {
        Some(c) => c,
        None => return,
    };
    publish(&ctx, customer_model()).await;
    let (_, rec) = call(
        &ctx.app,
        "POST",
        "/api/data/Customer",
        &ctx.token,
        Some(json!({"name":"Gone"}).to_string()),
        None,
    )
    .await;
    let id = rec["id"].as_str().unwrap().to_string();
    let _ = call(
        &ctx.app,
        "DELETE",
        &format!("/api/data/Customer/{id}"),
        &ctx.token,
        None,
        None,
    )
    .await;

    // admin (superuser) still sees the full history incl. the delete entry.
    let (st, body) = call(
        &ctx.app,
        "GET",
        &format!("/api/data/Customer/{id}/history"),
        &ctx.token,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "admin sees deleted history: {body}");
    let ops: Vec<&str> = body["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["op"].as_str().unwrap())
        .collect();
    assert!(ops.contains(&"delete"), "delete entry present: {body}");

    // as-of before deletion still reconstructs it.
    let (st, v1) = call(
        &ctx.app,
        "GET",
        &format!("/api/data/Customer/{id}/as-of?version=1"),
        &ctx.token,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v1["name"], "Gone");
}

#[tokio::test]
async fn error_responses_carry_stable_code() {
    // Regression guard for the §14 error-code taxonomy: every error envelope
    // carries a stable `code` (SDK/i18n key) + `status` + legacy `error`.
    let ctx = match setup().await {
        Some(c) => c,
        None => return,
    };
    publish(&ctx, customer_model()).await;

    // missing If-Match on a PATCH → 422 mda.invalid.
    let (_, rec) = call(
        &ctx.app,
        "POST",
        "/api/data/Customer",
        &ctx.token,
        Some(json!({"name":"X"}).to_string()),
        None,
    )
    .await;
    let id = rec["id"].as_str().unwrap().to_string();
    let (st, body) = call(
        &ctx.app,
        "PATCH",
        &format!("/api/data/Customer/{id}"),
        &ctx.token,
        Some(json!({"name":"Y"}).to_string()),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["code"], "mda.invalid");
    assert_eq!(body["status"], 422);
    assert_eq!(body["error"], "invalid");

    // wrong version → 409 mda.conflict.
    let (st, body) = call(
        &ctx.app,
        "PATCH",
        &format!("/api/data/Customer/{id}"),
        &ctx.token,
        Some(json!({"name":"Y"}).to_string()),
        Some(999),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(body["code"], "mda.conflict");
    assert_eq!(body["status"], 409);

    // unknown record → 404 mda.not_found.
    let (st, body) = call(
        &ctx.app,
        "GET",
        &format!("/api/data/Customer/{}", Uuid::new_v4()),
        &ctx.token,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "mda.not_found");
}

#[tokio::test]
async fn validation_returns_per_field_details() {
    // ADR-0018 refinement: a record write failing several field rules returns a
    // single `mda.validation` envelope whose `details` lists every problem
    // (field + stable code) in one round trip — not one fail-then-retry per field.
    let ctx = match setup().await {
        Some(c) => c,
        None => return,
    };
    publish(&ctx, customer_model()).await;

    // missing required `name`, a bad type on `balance`, and an unknown field.
    let (st, body) = call(
        &ctx.app,
        "POST",
        "/api/data/Customer",
        &ctx.token,
        Some(json!({"balance":"not-a-number","bogus":1}).to_string()),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["code"], "mda.validation");
    assert_eq!(body["error"], "validation");
    let details = body["details"].as_array().expect("details array");
    let by_field: std::collections::HashMap<&str, &str> = details
        .iter()
        .map(|d| (d["field"].as_str().unwrap(), d["code"].as_str().unwrap()))
        .collect();
    assert_eq!(by_field.get("name"), Some(&"mda.required"));
    assert_eq!(by_field.get("balance"), Some(&"mda.invalid_type"));
    assert_eq!(by_field.get("bogus"), Some(&"mda.unknown_field"));
}

// ===== team-OWD (ADR-0013 `owd_visible`) =====
//
// A helper to create a team + assign a user to it, and to set the OWD for an
// entity, all under the tenant GUC (the sec_* tables are RLS-gated).

/// Seed a team in the tenant, returning its id.
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

/// Put a user in a team (under the tenant GUC).
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

/// Set the OWD for an entity (under the tenant GUC).
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

/// Make `child` a sub-team of `parent` (under the tenant GUC). This is the
/// ADR-0013 team-hierarchy edge: a member of an ancestor team reads records
/// owned by members of any descendant team.
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

#[tokio::test]
async fn team_owd_lets_teammates_read_owners_record() {
    // OWD = team: members of the owner's team may read each other's records
    // (write stays owner-only, mirroring PublicRead). A user in a *different*
    // team (or no team) cannot.
    let ctx = match setup().await {
        Some(c) => c,
        None => return,
    };
    publish(&ctx, customer_model()).await;
    seed_owd(&ctx.pool, ctx.tenant, "Customer", "team").await;

    let team_a = seed_team(&ctx.pool, ctx.tenant, "team-a").await;
    let team_b = seed_team(&ctx.pool, ctx.tenant, "team-b").await;

    // owner: in team-a, can create + read its own records.
    let (owner_token, owner_id) =
        limited_user(&ctx, &[("Customer", "create"), ("Customer", "read")]).await;
    assign_team(&ctx.pool, ctx.tenant, owner_id, team_a).await;
    // teammate: in team-a, read only.
    let (mate_token, _mate_id) = limited_user(&ctx, &[("Customer", "read")]).await;
    assign_team(&ctx.pool, ctx.tenant, _mate_id, team_a).await;
    // outsider: in team-b, read only — different team.
    let (other_token, other_id) = limited_user(&ctx, &[("Customer", "read")]).await;
    assign_team(&ctx.pool, ctx.tenant, other_id, team_b).await;
    // team-less reader: read perm but no team at all.
    let (noteam_token, _noteam_id) = limited_user(&ctx, &[("Customer", "read")]).await;

    // owner creates a private-to-team record.
    let (_, rec) = call(
        &ctx.app,
        "POST",
        "/api/data/Customer",
        &owner_token,
        Some(json!({"name":"Shared-by-team"}).to_string()),
        None,
    )
    .await;
    let id = rec["id"].as_str().unwrap().to_string();

    // teammate (same team) can read it + sees it in the list.
    let (st, got) = call(
        &ctx.app,
        "GET",
        &format!("/api/data/Customer/{id}"),
        &mate_token,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "teammate can read: {got}");
    assert_eq!(got["name"], "Shared-by-team");
    let (_, list) = call(
        &ctx.app,
        "GET",
        "/api/data/Customer",
        &mate_token,
        None,
        None,
    )
    .await;
    assert_eq!(
        list["total"], 1,
        "teammate list sees the team record: {list}"
    );

    // outsider (different team) cannot read it (404) nor list it.
    let (st, _) = call(
        &ctx.app,
        "GET",
        &format!("/api/data/Customer/{id}"),
        &other_token,
        None,
        None,
    )
    .await;
    assert_eq!(
        st,
        StatusCode::NOT_FOUND,
        "different-team reader is blocked"
    );
    let (_, list) = call(
        &ctx.app,
        "GET",
        "/api/data/Customer",
        &other_token,
        None,
        None,
    )
    .await;
    assert_eq!(list["total"], 0, "different-team list is empty: {list}");

    // a team-less reader (read perm, but no team) is also blocked.
    let (st, _) = call(
        &ctx.app,
        "GET",
        &format!("/api/data/Customer/{id}"),
        &noteam_token,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND, "team-less reader is blocked");

    // team-OWD grants read, NOT write: the teammate cannot update (404) even
    // with the right object perm, because write is owner-only under team-OWD.
    let (mate_write_token, mate_write_id) =
        limited_user(&ctx, &[("Customer", "read"), ("Customer", "update")]).await;
    assign_team(&ctx.pool, ctx.tenant, mate_write_id, team_a).await;
    let (st, body) = call(
        &ctx.app,
        "PATCH",
        &format!("/api/data/Customer/{id}"),
        &mate_write_token,
        Some(json!({"name":"mutated"}).to_string()),
        rec["version"].as_i64(),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::NOT_FOUND,
        "team-OWD is read-only: write blocked: {body}"
    );
}

// ===== team hierarchy (ADR-0013 sub-team visibility) =====
//
// The `parent_id` edge on `sec_team`: a member of an ancestor (manager) team
// reads records owned by members of any descendant team; the reverse is denied,
// write stays owner-only, and a sibling branch is isolated.

#[tokio::test]
async fn team_hierarchy_let_ancestor_read_descendant_record() {
    // Tree:  parent
    //          └─ child-a
    //          └─ child-b
    // A record owned in `child-a` is readable by a member of `child-a` (same
    // team) AND by a member of `parent` (ancestor / manager). It is NOT readable
    // by a member of `child-b` (sibling branch) nor by a member of `child-a`
    // trying to read up into `parent` (visibility flows downward only).
    let ctx = match setup().await {
        Some(c) => c,
        None => return,
    };
    publish(&ctx, customer_model()).await;
    seed_owd(&ctx.pool, ctx.tenant, "Customer", "team").await;

    let parent = seed_team(&ctx.pool, ctx.tenant, "parent").await;
    let child_a = seed_team(&ctx.pool, ctx.tenant, "child-a").await;
    let child_b = seed_team(&ctx.pool, ctx.tenant, "child-b").await;
    set_team_parent(&ctx.pool, ctx.tenant, child_a, parent).await;
    set_team_parent(&ctx.pool, ctx.tenant, child_b, parent).await;

    // owner: in child-a, creates a record.
    let (owner_token, owner_id) =
        limited_user(&ctx, &[("Customer", "create"), ("Customer", "read")]).await;
    assign_team(&ctx.pool, ctx.tenant, owner_id, child_a).await;
    // manager: in parent (ancestor), read only.
    let (mgr_token, mgr_id) = limited_user(&ctx, &[("Customer", "read")]).await;
    assign_team(&ctx.pool, ctx.tenant, mgr_id, parent).await;
    // sibling: in child-b (different branch), read only.
    let (sib_token, sib_id) = limited_user(&ctx, &[("Customer", "read")]).await;
    assign_team(&ctx.pool, ctx.tenant, sib_id, child_b).await;
    // sub-member: in child-a, but tries to read a record owned in `parent`.
    let (sub_token, sub_id) =
        limited_user(&ctx, &[("Customer", "create"), ("Customer", "read")]).await;
    assign_team(&ctx.pool, ctx.tenant, sub_id, child_a).await;

    // owner creates a record in child-a.
    let (_, rec) = call(
        &ctx.app,
        "POST",
        "/api/data/Customer",
        &owner_token,
        Some(json!({"name":"Owned-by-child-a"}).to_string()),
        None,
    )
    .await;
    let id = rec["id"].as_str().unwrap().to_string();

    // manager (ancestor `parent`) can read it + sees it in the list.
    let (st, got) = call(
        &ctx.app,
        "GET",
        &format!("/api/data/Customer/{id}"),
        &mgr_token,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "ancestor/manager can read: {got}");
    assert_eq!(got["name"], "Owned-by-child-a");
    let (_, list) = call(
        &ctx.app,
        "GET",
        "/api/data/Customer",
        &mgr_token,
        None,
        None,
    )
    .await;
    assert_eq!(
        list["total"], 1,
        "ancestor list sees the descendant record: {list}"
    );

    // sibling (child-b, different branch) cannot read it (404) nor list it.
    let (st, _) = call(
        &ctx.app,
        "GET",
        &format!("/api/data/Customer/{id}"),
        &sib_token,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND, "sibling branch is blocked");
    let (_, list) = call(
        &ctx.app,
        "GET",
        "/api/data/Customer",
        &sib_token,
        None,
        None,
    )
    .await;
    assert_eq!(list["total"], 0, "sibling list is empty: {list}");

    // visibility flows DOWNWARD only: a member of `child-a` cannot read a record
    // owned in `parent`. Have the sub-member create one in child-a, then a
    // manager-owned record in `parent`, and confirm the sub-member does NOT see
    // the parent-owned one. (Re-use owner_token: put owner into parent to make a
    // parent-owned record.)
    assign_team(&ctx.pool, ctx.tenant, owner_id, parent).await;
    let (_, parent_rec) = call(
        &ctx.app,
        "POST",
        "/api/data/Customer",
        &owner_token,
        Some(json!({"name":"Owned-by-parent"}).to_string()),
        None,
    )
    .await;
    let parent_id = parent_rec["id"].as_str().unwrap().to_string();
    // sub-member is in child-a (a descendant of parent); it must NOT read the
    // parent-owned record (descendants cannot read up).
    let (st, _) = call(
        &ctx.app,
        "GET",
        &format!("/api/data/Customer/{parent_id}"),
        &sub_token,
        None,
        None,
    )
    .await;
    assert_eq!(
        st,
        StatusCode::NOT_FOUND,
        "descendant cannot read up into ancestor's record"
    );

    // write stays owner-only: the manager (ancestor) cannot update the
    // child-a-owned record even with the update perm.
    let (mgr_write_token, mgr_write_id) =
        limited_user(&ctx, &[("Customer", "read"), ("Customer", "update")]).await;
    assign_team(&ctx.pool, ctx.tenant, mgr_write_id, parent).await;
    let (st, body) = call(
        &ctx.app,
        "PATCH",
        &format!("/api/data/Customer/{id}"),
        &mgr_write_token,
        Some(json!({"name":"mutated-by-manager"}).to_string()),
        rec["version"].as_i64(),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::NOT_FOUND,
        "hierarchy grants read only: manager write blocked: {body}"
    );
}
