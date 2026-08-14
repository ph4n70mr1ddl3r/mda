//! Phase 8 authoring APIs: rules (`/api/rules`) and workflows
//! (`/api/workflows`) — the Studio's rule editor + workflow designer surface.
//! Author-time validation (entity/event/field/expression) is the contract: a
//! malformed rule or half-edited state machine must fail at author time, and a
//! valid one must actually fire when a record is written / transitioned.

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
}

fn ticket_model() -> Value {
    json!({
        "modules": [],
        "entities": [{
            "id": Uuid::new_v4(),
            "module_id": null,
            "name": "Ticket",
            "table_name": format!("ticket_{}", Uuid::new_v4().simple()),
            "label": "Ticket",
            "description": null,
            "fields": [
                {"id": Uuid::new_v4(), "name":"title","label":"Title","field_type":"string","required":true,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{}},
                {"id": Uuid::new_v4(), "name":"status","label":"Status","field_type":"enum","required":false,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{"options":["Open","Closed"]}},
                {"id": Uuid::new_v4(), "name":"closed_at","label":"Closed at","field_type":"datetime","required":false,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{}}
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
    })
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    token: &str,
    body: Option<String>,
) -> (StatusCode, Value) {
    call_im(app, method, uri, token, body, None).await
}

/// Like [`call`] but with an optional `If-Match` header (transition OCC).
async fn call_im(
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

// ===== rules =====

#[tokio::test]
async fn rule_authoring_validates_and_fires() {
    let ctx = match setup().await {
        Some(c) => c,
        None => return,
    };
    publish(&ctx, ticket_model()).await;

    // bad event -> 400 at author time
    let (st, _) = call(
        &ctx.app,
        "POST",
        "/api/rules",
        &ctx.admin_token,
        Some(
            json!({
                "entity":"Ticket", "event":"on_thursday",
                "condition":{"op":"Lit","value":true},
                "action_field":"closed_at", "action_value":{"op":"Lit","value":null}
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);

    // unknown action field -> 400
    let (st, _) = call(
        &ctx.app,
        "POST",
        "/api/rules",
        &ctx.admin_token,
        Some(
            json!({
                "entity":"Ticket", "event":"after_update",
                "condition":{"op":"Lit","value":true},
                "action_field":"nope", "action_value":{"op":"Lit","value":1}
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);

    // unparseable condition -> 400
    let (st, _) = call(
        &ctx.app,
        "POST",
        "/api/rules",
        &ctx.admin_token,
        Some(
            json!({
                "entity":"Ticket", "event":"after_update",
                "condition":{"op":"Bogus","x":1},
                "action_field":"closed_at", "action_value":{"op":"Lit","value":null}
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);

    // valid rule: when status becomes Closed, stamp closed_at = now()
    let (st, rule) = call(
        &ctx.app,
        "POST",
        "/api/rules",
        &ctx.admin_token,
        Some(
            json!({
                "entity":"Ticket", "event":"after_update",
                "condition":{"op":"Cmp","kind":"eq",
                    "lhs":{"op":"Field","name":"status"},
                    "rhs":{"op":"Lit","value":"Closed"}},
                "action_field":"closed_at",
                "action_value":{"op":"Call","name":"now","args":[]}
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let rule_id = rule["id"].as_str().unwrap().to_string();

    // list shows it
    let (st, list) = call(&ctx.app, "GET", "/api/rules", &ctx.admin_token, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 1);

    // it fires on a real write: create a ticket, close it, closed_at is set
    let (st, rec) = call(
        &ctx.app,
        "POST",
        "/api/data/Ticket",
        &ctx.admin_token,
        Some(json!({"title":"t"}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let id = rec["id"].as_str().unwrap().to_string();
    let ver = rec["version"].as_i64().unwrap();
    let (st, updated) = call_im(
        &ctx.app,
        "PATCH",
        &format!("/api/data/Ticket/{id}"),
        &ctx.admin_token,
        Some(json!({"status":"Closed"}).to_string()),
        Some(ver),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "update failed: {updated}");
    assert!(
        updated["closed_at"].is_string(),
        "rule did not fire: {updated}"
    );

    // deactivate -> no longer fires
    let (st, _) = call(
        &ctx.app,
        "PATCH",
        &format!("/api/rules/{rule_id}"),
        &ctx.admin_token,
        Some(json!({"active":false}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (_, list) = call(&ctx.app, "GET", "/api/rules", &ctx.admin_token, None).await;
    assert_eq!(list[0]["active"], json!(false));

    // delete -> gone
    let (st, _) = call(
        &ctx.app,
        "DELETE",
        &format!("/api/rules/{rule_id}"),
        &ctx.admin_token,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (_, list) = call(&ctx.app, "GET", "/api/rules", &ctx.admin_token, None).await;
    assert_eq!(list.as_array().unwrap().len(), 0);

    let _ = ver;
}

// ===== workflows =====

#[tokio::test]
async fn workflow_authoring_validates_and_runs() {
    let ctx = match setup().await {
        Some(c) => c,
        None => return,
    };
    publish(&ctx, ticket_model()).await;

    // transition to an undeclared state -> 400 (whole machine rejected)
    let (st, _) = call(
        &ctx.app,
        "POST",
        "/api/workflows",
        &ctx.admin_token,
        Some(
            json!({
                "entity":"Ticket", "name":"lifecycle",
                "states":["active","Closed"],
                "transitions":[{"name":"close","from_state":"Open","to_state":"Vanished"}]
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);

    // unknown entity -> 400
    let (st, _) = call(
        &ctx.app,
        "POST",
        "/api/workflows",
        &ctx.admin_token,
        Some(json!({"entity":"Ghost","name":"g","states":["A"]}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);

    // valid machine with a guard + action: close sets closed_at = now()
    let (st, wf) = call(
        &ctx.app,
        "POST",
        "/api/workflows",
        &ctx.admin_token,
        Some(
            json!({
                "entity":"Ticket", "name":"lifecycle",
                "states":["active","Closed"],
                "transitions":[
                    {"name":"close", "from_state":"active", "to_state":"Closed",
                     "guard":{"op":"Cmp","kind":"ne",
                              "lhs":{"op":"Field","name":"title"},
                              "rhs":{"op":"Lit","value":""}},
                     "actions":[{"field":"closed_at","value":{"op":"Call","name":"now","args":[]}}],
                     "creates_task":false},
                    {"name":"reopen", "from_state":"Closed", "to_state":"active"}
                ]
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let wf_id = wf["id"].as_str().unwrap().to_string();

    // list renders the whole graph
    let (st, list) = call(&ctx.app, "GET", "/api/workflows", &ctx.admin_token, None).await;
    assert_eq!(st, StatusCode::OK);
    let wf = &list[0];
    assert_eq!(wf["states"], json!(["active", "Closed"]));
    assert_eq!(wf["transitions"].as_array().unwrap().len(), 2);

    // run it end-to-end: create → close (guard passes, action stamps closed_at) → reopen
    let (_, rec) = call(
        &ctx.app,
        "POST",
        "/api/data/Ticket",
        &ctx.admin_token,
        Some(json!({"title":"has title"}).to_string()),
    )
    .await;
    let id = rec["id"].as_str().unwrap().to_string();
    let ver = rec["version"].as_i64().unwrap();
    let (st, closed) = call_im(
        &ctx.app,
        "POST",
        &format!("/api/data/Ticket/{id}/close"),
        &ctx.admin_token,
        None,
        Some(ver),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "close failed: {closed}");
    assert_eq!(closed["state"], json!("Closed"));
    assert!(
        closed["closed_at"].is_string(),
        "transition action did not run: {closed}"
    );

    let ver2 = closed["version"].as_i64().unwrap();
    let (st, _) = call_im(
        &ctx.app,
        "POST",
        &format!("/api/data/Ticket/{id}/reopen"),
        &ctx.admin_token,
        None,
        Some(ver2),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    // deactivate → transitions 404 the machine
    let (st, _) = call(
        &ctx.app,
        "PATCH",
        &format!("/api/workflows/{wf_id}"),
        &ctx.admin_token,
        Some(json!({"active":false}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (_, rec2) = call(
        &ctx.app,
        "GET",
        &format!("/api/data/Ticket/{id}"),
        &ctx.admin_token,
        None,
    )
    .await;
    let ver3 = rec2["version"].as_i64().unwrap();
    let (st, _) = call_im(
        &ctx.app,
        "POST",
        &format!("/api/data/Ticket/{id}/close"),
        &ctx.admin_token,
        None,
        Some(ver3),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // delete → gone (and cascades states/transitions)
    let (st, _) = call(
        &ctx.app,
        "DELETE",
        &format!("/api/workflows/{wf_id}"),
        &ctx.admin_token,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (_, list) = call(&ctx.app, "GET", "/api/workflows", &ctx.admin_token, None).await;
    assert_eq!(list.as_array().unwrap().len(), 0);
    let (n,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM meta.md_workflow_transition tr \
         JOIN meta.md_workflow w ON w.id = tr.workflow_id WHERE w.tenant_id = $1",
    )
    .bind(ctx.tenant)
    .fetch_one(&ctx.pool)
    .await
    .unwrap();
    assert_eq!(n, 0);
}
