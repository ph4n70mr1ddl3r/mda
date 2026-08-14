//! Phase 1 end-to-end (auth-aware): branch → validate → publish, with the draft
//! `If-Match` etag and the additive-only checks. Each test spins up a fresh
//! tenant + superuser and authenticates with a bearer token.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use mda_api::AppState;
use mda_meta::MetadataCache;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;
use uuid::Uuid;

mod common;

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
                {"id": Uuid::new_v4(), "name":"tier","label":"Tier","field_type":"enum","required":false,"is_unique":false,"is_indexed":true,"default_expr":null,"config":{"options":["Bronze","Silver","Gold"]}}
            ],
            "relationships": []
        }]
    })
}

/// Fresh tenant + superuser -> (app, bearer token).
async fn setup() -> Option<(axum::Router, String)> {
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
    let user_id = common::seed_user(&pool, tenant, &email, "tester", &hash).await;
    common::seed_assignment(&pool, tenant, user_id, role_id).await;
    let jwt = mda_security::JwtConfig::from_env();
    let token = jwt.issue_access(user_id, tenant, None).unwrap();
    let blobs: std::sync::Arc<dyn mda_api::blobs::BlobStore> =
        std::sync::Arc::new(mda_api::blobs::LocalBlobStore::from_env());
    let secrets: std::sync::Arc<dyn mda_core::SecretStore> =
        std::sync::Arc::new(mda_api::secrets::LocalSecretStore::from_env());
    // Publish DDL + meta writes run as the non-superuser `mda_app` role (the
    // role the app uses in production), matching prod ownership of biz tables.
    let app_pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&app_role_url(&db_url))
        .await
        .unwrap_or_else(|e| {
            eprintln!("could not connect as mda_app ({e}); using owner pool");
            pool
        });
    let app = mda_api::router(AppState {
        pool: app_pool,
        cache: MetadataCache::new(),
        jwt,
        blobs,

        secrets,
        events: mda_api::events::channel(),
        login_throttle: mda_security::LoginThrottle::default(),
        gql: std::sync::Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
    });
    Some((app, token))
}

/// Swap the userinfo of `url` to connect as the non-superuser `mda_app` role.
fn app_role_url(url: &str) -> String {
    if let Some(scheme_end) = url.find("://") {
        let rest = &url[scheme_end + 3..];
        if let Some(at) = rest.find('@') {
            return format!("{}://mda_app:mda@{}", &url[..scheme_end], &rest[at + 1..]);
        }
    }
    url.to_string()
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    token: &str,
    body: Option<String>,
    if_match: Option<String>,
) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(uri);
    b = b.header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"));
    if let Some(v) = if_match {
        b = b.header("if-match", v);
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

#[tokio::test]
async fn branch_validate_publish_reflects_in_model_and_cache() {
    let (app, token) = match setup().await {
        Some(x) => x,
        None => return,
    };

    let (st, draft) = call(
        &app,
        "POST",
        "/api/studio/drafts",
        &token,
        Some(json!({"name":"v1"}).to_string()),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "branch: {draft}");
    let draft_id = draft["id"].as_str().unwrap().parse::<Uuid>().unwrap();
    let etag = draft["version_etag"].as_str().unwrap();
    assert_eq!(draft["status"], "draft");

    let (st, body) = call(
        &app,
        "PUT",
        &format!("/api/studio/drafts/{draft_id}/model"),
        &token,
        Some(customer_model().to_string()),
        Some(etag.to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "put_model: {body}");

    let (st, report) = call(
        &app,
        "POST",
        &format!("/api/studio/drafts/{draft_id}/validate"),
        &token,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "validate: {report}");
    assert_eq!(report["valid"], true, "report: {report}");
    assert_eq!(report["additions"]["entities"], 1);

    let (st, res) = call(
        &app,
        "POST",
        &format!("/api/studio/drafts/{draft_id}/publish"),
        &token,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "publish: {res}");
    assert_eq!(res["version"], 1);

    let (st, model) = call(&app, "GET", "/api/studio/model", &token, None, None).await;
    assert_eq!(st, StatusCode::OK);
    let entity_id = model["entities"][0]["id"]
        .as_str()
        .unwrap()
        .parse::<Uuid>()
        .unwrap();

    let (st, def) = call(
        &app,
        "GET",
        &format!("/api/studio/entities/{entity_id}"),
        &token,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "cache get: {def}");
    assert_eq!(def["entity"]["name"], "Customer");
}

#[tokio::test]
async fn transform_is_rejected() {
    let (app, token) = match setup().await {
        Some(x) => x,
        None => return,
    };

    // publish a Customer first
    let (_, draft) = call(
        &app,
        "POST",
        "/api/studio/drafts",
        &token,
        Some(json!({"name":"a"}).to_string()),
        None,
    )
    .await;
    let d1 = draft["id"].as_str().unwrap().parse::<Uuid>().unwrap();
    let e1 = draft["version_etag"].as_str().unwrap();
    let _ = call(
        &app,
        "PUT",
        &format!("/api/studio/drafts/{d1}/model"),
        &token,
        Some(customer_model().to_string()),
        Some(e1.to_string()),
    )
    .await;
    let _ = call(
        &app,
        "POST",
        &format!("/api/studio/drafts/{d1}/publish"),
        &token,
        None,
        None,
    )
    .await;

    // mutate a field's type (a transform)
    let (_, active) = call(&app, "GET", "/api/studio/model", &token, None, None).await;
    let mut mutated = active.clone();
    mutated["entities"][0]["fields"][0]["field_type"] = json!("integer");

    let (_, draft2) = call(
        &app,
        "POST",
        "/api/studio/drafts",
        &token,
        Some(json!({"name":"b"}).to_string()),
        None,
    )
    .await;
    let d2 = draft2["id"].as_str().unwrap().parse::<Uuid>().unwrap();
    let e2 = draft2["version_etag"].as_str().unwrap();
    let (st, _) = call(
        &app,
        "PUT",
        &format!("/api/studio/drafts/{d2}/model"),
        &token,
        Some(mutated.to_string()),
        Some(e2.to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    let (st, report) = call(
        &app,
        "POST",
        &format!("/api/studio/drafts/{d2}/validate"),
        &token,
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

    let (st, _) = call(
        &app,
        "POST",
        &format!("/api/studio/drafts/{d2}/publish"),
        &token,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn etag_conflict_returns_409() {
    let (app, token) = match setup().await {
        Some(x) => x,
        None => return,
    };

    let (_, draft) = call(
        &app,
        "POST",
        "/api/studio/drafts",
        &token,
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
        &token,
        Some(customer_model().to_string()),
        Some(Uuid::new_v4().to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);

    // missing If-Match entirely -> 422
    let (st, _) = call(
        &app,
        "PUT",
        &format!("/api/studio/drafts/{id}/model"),
        &token,
        Some(customer_model().to_string()),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn drafts_list_and_discard() {
    let (app, token) = match setup().await {
        Some(x) => x,
        None => return,
    };

    // two drafts (one to discard, one to publish)
    let (_, d1) = call(
        &app,
        "POST",
        "/api/studio/drafts",
        &token,
        Some(json!({"name":"scratch"}).to_string()),
        None,
    )
    .await;
    let (_, d2) = call(
        &app,
        "POST",
        "/api/studio/drafts",
        &token,
        Some(json!({"name":"real"}).to_string()),
        None,
    )
    .await;
    let id1 = d1["id"].as_str().unwrap().parse::<Uuid>().unwrap();
    let id2 = d2["id"].as_str().unwrap().parse::<Uuid>().unwrap();

    // list contains both, newest first, and carries no model blob
    let (st, list) = call(&app, "GET", "/api/studio/drafts", &token, None, None).await;
    assert_eq!(st, StatusCode::OK);
    let ids: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&id1.to_string().as_str()));
    assert!(ids.contains(&id2.to_string().as_str()));
    assert!(list[0].get("model").is_none());

    // publish d2, then discard d1
    let etag2 = d2["version_etag"].as_str().unwrap().to_string();
    let (st, _) = call(
        &app,
        "PUT",
        &format!("/api/studio/drafts/{id2}/model"),
        &token,
        Some(customer_model().to_string()),
        Some(etag2),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (st, _) = call(
        &app,
        "POST",
        &format!("/api/studio/drafts/{id2}/publish"),
        &token,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    let (st, _) = call(
        &app,
        "DELETE",
        &format!("/api/studio/drafts/{id1}"),
        &token,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (_, list) = call(&app, "GET", "/api/studio/drafts", &token, None, None).await;
    assert_eq!(list.as_array().unwrap().len(), 1);

    // a published draft is history — discarding it is a 409
    let (st, _) = call(
        &app,
        "DELETE",
        &format!("/api/studio/drafts/{id2}"),
        &token,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);

    // discarding an unknown id is a 404
    let (st, _) = call(
        &app,
        "DELETE",
        &format!("/api/studio/drafts/{}", Uuid::new_v4()),
        &token,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}
