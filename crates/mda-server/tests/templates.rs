//! Templating (PLAN §5.19): sandboxed render + AuthZ-by-construction record mode
//! + locale best-match resolution.

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
}

fn customer_model(table: &str) -> Value {
    json!({
        "modules": [],
        "entities": [{
            "id": Uuid::new_v4(),
            "module_id": null,
            "name": "Customer",
            "table_name": table,
            "label": "Customer",
            "description": null,
            "fields": [
                {"id": Uuid::new_v4(), "name":"name","label":"Name","field_type":"string","required":true,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{}},
                {"id": Uuid::new_v4(), "name":"tier","label":"Tier","field_type":"enum","required":false,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{"options":["Bronze","Silver","Gold"]}},
                {"id": Uuid::new_v4(), "name":"secret","label":"Secret","field_type":"string","required":false,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{}}
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

#[tokio::test]
async fn template_renders_context_and_record_modes() {
    let Some(ctx) = setup().await else {
        return;
    };
    let table = format!("cust_{}", Uuid::new_v4().simple());
    publish(&ctx, customer_model(&table)).await;

    // create a Customer record.
    let (_, r) = call(
        &ctx.app,
        "POST",
        "/api/data/Customer",
        &ctx.token,
        Some(json!({"name":"Acme","tier":"Gold","secret":"shh"}).to_string()),
    )
    .await;
    let id = r["id"].as_str().unwrap().parse::<Uuid>().unwrap();

    // (1) context mode: explicit context.
    let (st, _v) = call(
        &ctx.app,
        "POST",
        "/api/templates",
        &ctx.token,
        Some(
            json!({"name":"welcome","kind":"email","content_type":"text/html",
                    "body":"<p>Welcome, {{ who }}!</p>"})
            .to_string(),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);

    let (st, v) = call(
        &ctx.app,
        "POST",
        "/api/templates/welcome/render",
        &ctx.token,
        Some(json!({"context":{"who":"Ada"}}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["body"], "<p>Welcome, Ada!</p>");
    assert_eq!(v["content_type"], "text/html");

    // (2) record mode: loads + AuthZ-projects the record.
    let (st, v) = call(
        &ctx.app,
        "POST",
        &format!("/api/templates/welcome/render?entity=Customer&id={id}"),
        &ctx.token,
        Some(json!({"context":{}}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    // The template references {{ who }} which the record context doesn't have →
    // renders empty. The point is the record-loaded render path works.
    assert_eq!(v["body"], "<p>Welcome, !</p>");

    // A record-tied template body interpolates the record fields.
    let (_, _) = call(
        &ctx.app,
        "POST",
        "/api/templates",
        &ctx.token,
        Some(
            json!({"name":"cust_card","body":"{{ record.name }} is {{ record.tier }}"}).to_string(),
        ),
    )
    .await;
    let (st, v) = call(
        &ctx.app,
        "POST",
        &format!("/api/templates/cust_card/render?entity=Customer&id={id}"),
        &ctx.token,
        Some(json!({}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["body"], "Acme is Gold");
}

#[tokio::test]
async fn template_locale_best_match() {
    let Some(ctx) = setup().await else {
        return;
    };
    // default (NULL locale), plus en-US and fr-FR variants.
    for (locale, body) in [
        (None, "Hello {{ who }}"),
        (Some("en-US"), "Hi {{ who }}"),
        (Some("fr-FR"), "Bonjour {{ who }}"),
    ] {
        let (_, _) = call(
            &ctx.app,
            "POST",
            "/api/templates",
            &ctx.token,
            Some(json!({"name":"greet","body":body,"locale":locale}).to_string()),
        )
        .await;
    }

    // exact match
    let (st, v) = call(
        &ctx.app,
        "POST",
        "/api/templates/greet/render?locale=fr-FR",
        &ctx.token,
        Some(json!({"context":{"who":"Ada"}}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["body"], "Bonjour Ada");

    // language-prefix fallback (en-GB → en-US? no — prefix is "en", no "en"
    // variant exists, so it falls through to the NULL default).
    let (st, v) = call(
        &ctx.app,
        "POST",
        "/api/templates/greet/render?locale=en-GB",
        &ctx.token,
        Some(json!({"context":{"who":"Ada"}}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["body"], "Hello Ada");

    // no locale requested → default
    let (_, v) = call(
        &ctx.app,
        "POST",
        "/api/templates/greet/render",
        &ctx.token,
        Some(json!({"context":{"who":"Ada"}}).to_string()),
    )
    .await;
    assert_eq!(v["body"], "Hello Ada");
}
