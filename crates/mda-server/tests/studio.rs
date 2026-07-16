//! Phase 1 end-to-end test (the deliverable): branch a draft, add a "Customer"
//! entity, validate, publish, and confirm the active model + the cache reflect
//! it — plus the additive-only / etag-concurrency negative cases.
//!
//! Runs in-process against `mda_api::router` (no port). Each test uses a unique
//! tenant id so they're isolated and parallel-safe. Skipped without DATABASE_URL.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use mda_api::AppState;
use mda_meta::MetadataCache;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;
use uuid::Uuid;

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
                {"id": Uuid::new_v4(), "name":"email","label":"Email","field_type":"string","required":false,"is_unique":true,"is_indexed":true,"default_expr":null,"config":{}},
                {"id": Uuid::new_v4(), "name":"tier","label":"Tier","field_type":"enum","required":false,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{"options":["Bronze","Silver","Gold"]}}
            ],
            "relationships": []
        }]
    })
}

async fn app(pool: sqlx::PgPool) -> axum::Router {
    mda_api::router(AppState {
        pool,
        cache: MetadataCache::new(),
    })
}

/// Issue a request, optionally with a body and an `If-Match` etag.
async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    tenant: Uuid,
    body: Option<String>,
    if_match: Option<Uuid>,
) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(uri);
    b = b.header("x-tenant-id", tenant.to_string());
    if let Some(etag) = if_match {
        b = b.header("if-match", etag.to_string());
    }
    let req = if let Some(json_body) = body {
        b.header("content-type", "application/json")
            .body(Body::from(json_body))
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

async fn setup() -> Option<(axum::Router,)> {
    let url = match std::env::var("DATABASE_URL") {
        Ok(u) => u,
        Err(_) => {
            eprintln!("skipping: DATABASE_URL not set");
            return None;
        }
    };
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .unwrap();
    mda_server::migrate::run(&pool).await.unwrap();
    Some((app(pool).await,))
}

#[tokio::test]
async fn branch_validate_publish_reflects_in_model_and_cache() {
    let (app,) = match setup().await {
        Some(x) => x,
        None => return,
    };
    let tenant = Uuid::new_v4();

    // 1. branch a draft from (empty) active model
    let (st, draft) = call(
        &app,
        "POST",
        "/api/studio/drafts",
        tenant,
        Some(json!({"name":"v1"}).to_string()),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "branch: {draft}");
    let draft_id = draft["id"].as_str().unwrap().parse::<Uuid>().unwrap();
    let etag = draft["version_etag"]
        .as_str()
        .unwrap()
        .parse::<Uuid>()
        .unwrap();
    assert_eq!(draft["status"], "draft");
    assert!(draft["model"]["entities"].as_array().unwrap().is_empty());

    // 2. edit: put the Customer model with If-Match etag
    let (st, body) = call(
        &app,
        "PUT",
        &format!("/api/studio/drafts/{draft_id}/model"),
        tenant,
        Some(customer_model().to_string()),
        Some(etag),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "put_model: {body}");
    assert_ne!(
        body["version_etag"]
            .as_str()
            .unwrap()
            .parse::<Uuid>()
            .unwrap(),
        etag
    );

    // 3. validate -> additive, valid
    let (st, report) = call(
        &app,
        "POST",
        &format!("/api/studio/drafts/{draft_id}/validate"),
        tenant,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "validate: {report}");
    assert_eq!(report["valid"], true, "report: {report}");
    assert_eq!(report["additions"]["entities"], 1);

    // 4. publish
    let (st, res) = call(
        &app,
        "POST",
        &format!("/api/studio/drafts/{draft_id}/publish"),
        tenant,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "publish: {res}");
    assert_eq!(res["version"], 1);
    assert_eq!(res["additions"]["entities"], 1);

    // 5. active model now contains Customer
    let (st, model) = call(&app, "GET", "/api/studio/model", tenant, None, None).await;
    assert_eq!(st, StatusCode::OK);
    let ent = &model["entities"][0];
    assert_eq!(ent["name"], "Customer");
    assert_eq!(ent["fields"].as_array().unwrap().len(), 3);
    let entity_id = ent["id"].as_str().unwrap().parse::<Uuid>().unwrap();

    // 6. read through the cache -> returns the definition (cache path works post-publish)
    let (st, def) = call(
        &app,
        "GET",
        &format!("/api/studio/entities/{entity_id}"),
        tenant,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "cache get: {def}");
    assert_eq!(def["entity"]["name"], "Customer");
    assert_eq!(def["fields"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn transform_is_rejected() {
    let (app,) = match setup().await {
        Some(x) => x,
        None => return,
    };
    let tenant = Uuid::new_v4();

    // publish a Customer first
    let (_, draft) = call(
        &app,
        "POST",
        "/api/studio/drafts",
        tenant,
        Some(json!({"name":"a"}).to_string()),
        None,
    )
    .await;
    let d1 = draft["id"].as_str().unwrap().parse::<Uuid>().unwrap();
    let e1 = draft["version_etag"]
        .as_str()
        .unwrap()
        .parse::<Uuid>()
        .unwrap();
    let _ = call(
        &app,
        "PUT",
        &format!("/api/studio/drafts/{d1}/model"),
        tenant,
        Some(customer_model().to_string()),
        Some(e1),
    )
    .await;
    let _ = call(
        &app,
        "POST",
        &format!("/api/studio/drafts/{d1}/publish"),
        tenant,
        None,
        None,
    )
    .await;

    // read the active model, then mutate a field's type (a transform)
    let (_, active) = call(&app, "GET", "/api/studio/model", tenant, None, None).await;
    let mut mutated = active.clone();
    mutated["entities"][0]["fields"][0]["field_type"] = json!("integer");

    let (_, draft2) = call(
        &app,
        "POST",
        "/api/studio/drafts",
        tenant,
        Some(json!({"name":"b"}).to_string()),
        None,
    )
    .await;
    let d2 = draft2["id"].as_str().unwrap().parse::<Uuid>().unwrap();
    let e2 = draft2["version_etag"]
        .as_str()
        .unwrap()
        .parse::<Uuid>()
        .unwrap();
    let (st, _) = call(
        &app,
        "PUT",
        &format!("/api/studio/drafts/{d2}/model"),
        tenant,
        Some(mutated.to_string()),
        Some(e2),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    // validate flags the type change as a Phase-2 transform (not yet supported)
    let (st, report) = call(
        &app,
        "POST",
        &format!("/api/studio/drafts/{d2}/validate"),
        tenant,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(report["valid"], false, "expected invalid: {report}");
    assert!(report["violations"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| { v.as_str().unwrap().contains("modified") }));

    // publish is rejected (422) — ADR-0011 staged migration not yet implemented
    let (st, _) = call(
        &app,
        "POST",
        &format!("/api/studio/drafts/{d2}/publish"),
        tenant,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn etag_conflict_returns_409() {
    let (app,) = match setup().await {
        Some(x) => x,
        None => return,
    };
    let tenant = Uuid::new_v4();

    let (_, draft) = call(
        &app,
        "POST",
        "/api/studio/drafts",
        tenant,
        Some(json!({"name":"c"}).to_string()),
        None,
    )
    .await;
    let id = draft["id"].as_str().unwrap().parse::<Uuid>().unwrap();

    // PUT with a stale/foreign etag -> 409
    let (st, _) = call(
        &app,
        "PUT",
        &format!("/api/studio/drafts/{id}/model"),
        tenant,
        Some(customer_model().to_string()),
        Some(Uuid::new_v4()),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);

    // missing If-Match entirely -> 422
    let (st, _) = call(
        &app,
        "PUT",
        &format!("/api/studio/drafts/{id}/model"),
        tenant,
        Some(customer_model().to_string()),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
}
