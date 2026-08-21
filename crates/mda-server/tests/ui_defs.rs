//! UI definitions (Phase 6): forms, views, dashboards, navigation — the
//! renderable JSON the Runtime UI interprets, resolved against the active
//! model AND the caller's security (a definition can never widen access).

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

fn crm_model(customer_table: &str, region_table: &str) -> Value {
    let region_id = Uuid::new_v4();
    let customer_id = Uuid::new_v4();
    json!({
        "modules": [],
        "entities": [
            {
                "id": region_id, "module_id": null, "name": "Region",
                "table_name": region_table, "label": "Region", "description": null,
                "fields": [
                    {"id": Uuid::new_v4(), "name":"name","label":"Name","field_type":"string","required":true,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{}}
                ],
                "relationships": []
            },
            {
                "id": customer_id, "module_id": null, "name": "Customer",
                "table_name": customer_table, "label": "Customer", "description": null,
                "fields": [
                    {"id": Uuid::new_v4(), "name":"name","label":"Name","field_type":"string","required":true,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{}},
                    {"id": Uuid::new_v4(), "name":"tier","label":"Tier","field_type":"enum","required":false,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{"options":["Bronze","Silver","Gold"]}},
                    {"id": Uuid::new_v4(), "name":"notes","label":"Notes","field_type":"text","required":false,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{}},
                    {"id": Uuid::new_v4(), "name":"secret_score","label":"Score","field_type":"integer","required":false,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{}}
                ],
                "relationships": [
                    {"id": Uuid::new_v4(), "source_entity_id": customer_id, "source_field_name":"region_id",
                     "target_entity_id": region_id, "cardinality":"many_to_one", "strength":"lookup",
                     "on_delete":"restrict", "required": false, "reference_qualifier": null, "rollup_summary": null}
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

/// A user whose role reads Customer but is field-denied `secret_score`.
async fn seed_restricted_user(ctx: &Ctx) -> String {
    let hash = mda_security::hash_password("x").unwrap();
    let email = format!("u{}@test", Uuid::new_v4().simple());
    let id = common::seed_user(&ctx.pool, ctx.tenant, &email, "u", &hash).await;
    let role = common::seed_role(
        &ctx.pool,
        ctx.tenant,
        "viewer",
        &[("Customer", "read"), ("Region", "read")],
    )
    .await;
    common::seed_assignment(&ctx.pool, ctx.tenant, id, role).await;
    common::seed_field_permission(
        &ctx.pool,
        ctx.tenant,
        role,
        "Customer",
        "secret_score",
        "none",
    )
    .await;
    let jwt = JwtConfig::from_env();
    jwt.issue_access(id, ctx.tenant, None).unwrap()
}

async fn make_report(ctx: &Ctx, name: &str) -> Uuid {
    let (st, v) = call(
        &ctx.app,
        "POST",
        "/api/reports",
        &ctx.admin_token,
        Some(
            json!({
                "name": name,
                "dataset": {
                    "base_entity":"Customer",
                    "fields":[{"field":"name"},{"field":"tier"}],
                    "order_by":[{"field":"name","asc":true}],
                    "limit":10
                }
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "report: {v}");
    v["id"].as_str().unwrap().parse().unwrap()
}

#[tokio::test]
async fn forms_default_then_authored_and_fls_projection() {
    let Some(ctx) = setup().await else { return };
    publish(
        &ctx,
        crm_model(
            &format!("customer_{}", Uuid::new_v4().simple()),
            &format!("region_{}", Uuid::new_v4().simple()),
        ),
    )
    .await;

    // no stored form → synthesized default from the model (widget inference)
    let (st, v) = call(
        &ctx.app,
        "GET",
        "/api/forms/Customer",
        &ctx.admin_token,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let fields = v["sections"][0]["fields"].as_array().unwrap().clone();
    assert!(fields.len() >= 5, "scalars + the FK field: {fields:?}");
    let by_name = |n: &str| {
        fields
            .iter()
            .find(|f| f["name"] == n)
            .unwrap_or_else(|| panic!("field {n} missing"))
            .clone()
    };
    assert_eq!(by_name("tier")["widget"], "select");
    assert_eq!(by_name("notes")["widget"], "textarea");
    assert_eq!(by_name("region_id")["widget"], "reference");
    assert_eq!(
        by_name("tier")["options"],
        json!(["Bronze", "Silver", "Gold"])
    );

    // author a layout: only two fields, an override, a section title
    let (st, _) = call(
        &ctx.app,
        "POST",
        "/api/forms/Customer",
        &ctx.admin_token,
        Some(
            json!({
                "name": "quick",
                "label": "Quick edit",
                "layout": {"sections": [
                    {"title": "Identity", "fields": [
                        {"name": "tier", "widget": "text", "label": "Level"},
                        {"name": "name"}
                    ]}
                ]}
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let (st, v) = call(
        &ctx.app,
        "GET",
        "/api/forms/Customer?name=quick",
        &ctx.admin_token,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["label"], "Quick edit");
    let fields = v["sections"][0]["fields"].as_array().unwrap().clone();
    assert_eq!(fields.len(), 2);
    assert_eq!(fields[0]["label"], "Level", "author override kept");
    assert_eq!(fields[0]["type"], "enum", "model truth preserved");
    assert_eq!(fields[1]["required"], true);

    // malformed layout is rejected at author time
    let (st, _) = call(
        &ctx.app,
        "POST",
        "/api/forms/Customer",
        &ctx.admin_token,
        Some(
            json!({"name":"bad","layout":{"sections":[{"title":"x","fields":[{"nope":1}]}]}})
                .to_string(),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);

    // FLS: a field-denied user never sees secret_score in any form
    let token = seed_restricted_user(&ctx).await;
    let (st, v) = call(&ctx.app, "GET", "/api/forms/Customer", &token, None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let names: Vec<&str> = v["sections"][0]["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert!(!names.contains(&"secret_score"), "FLS drops denied field");
    assert!(names.contains(&"tier"));

    // delete the authored form → default again
    let (st, _) = call(
        &ctx.app,
        "DELETE",
        "/api/forms/Customer/quick",
        &ctx.admin_token,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (_, v) = call(
        &ctx.app,
        "GET",
        "/api/forms/Customer?name=quick",
        &ctx.admin_token,
        None,
    )
    .await;
    let names: Vec<&str> = v["sections"][0]["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"secret_score"), "default form back");
}

#[tokio::test]
async fn views_author_validate_and_fls_drop() {
    let Some(ctx) = setup().await else { return };
    publish(
        &ctx,
        crm_model(
            &format!("customer_{}", Uuid::new_v4().simple()),
            &format!("region_{}", Uuid::new_v4().simple()),
        ),
    )
    .await;

    // default view: first five fields with labels
    let (st, v) = call(
        &ctx.app,
        "GET",
        "/api/views/Customer",
        &ctx.admin_token,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(!v["columns"].as_array().unwrap().is_empty());
    assert_eq!(v["columns"][0]["label"], "Name");

    // authored view with an unknown field fails at author time
    let (st, v) = call(
        &ctx.app,
        "POST",
        "/api/views/Customer",
        &ctx.admin_token,
        Some(json!({"name":"grid","columns":[{"field":"nope"}]}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{v}");

    // authored view: subset + custom label + sort
    let (st, _) = call(
        &ctx.app,
        "POST",
        "/api/views/Customer",
        &ctx.admin_token,
        Some(
            json!({
                "name": "grid",
                "columns": [
                    {"field": "name", "label": "Account"},
                    {"field": "tier", "width": 120},
                    {"field": "secret_score"}
                ],
                "sort": [{"field": "name", "asc": true}],
                "page_size": 25
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let (st, v) = call(
        &ctx.app,
        "GET",
        "/api/views/Customer?name=grid",
        &ctx.admin_token,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["columns"][0]["label"], "Account");
    assert_eq!(v["page_size"], 25);

    // FLS drops the denied column for the restricted user
    let token = seed_restricted_user(&ctx).await;
    let (_, v) = call(
        &ctx.app,
        "GET",
        "/api/views/Customer?name=grid",
        &token,
        None,
    )
    .await;
    let cols: Vec<&str> = v["columns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["field"].as_str().unwrap())
        .collect();
    assert_eq!(cols, vec!["name", "tier"], "denied column dropped");
}

#[tokio::test]
async fn dashboards_run_reports_under_the_caller() {
    let Some(ctx) = setup().await else { return };
    publish(
        &ctx,
        crm_model(
            &format!("customer_{}", Uuid::new_v4().simple()),
            &format!("region_{}", Uuid::new_v4().simple()),
        ),
    )
    .await;
    for (name, tier) in [("Acme", "Gold"), ("Zeta", "Bronze")] {
        let (st, _) = call(
            &ctx.app,
            "POST",
            "/api/data/Customer",
            &ctx.admin_token,
            Some(json!({"name": name, "tier": tier}).to_string()),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
    }
    let report_id = make_report(&ctx, "customers").await;

    // author a dashboard tiling that report
    let (st, v) = call(
        &ctx.app,
        "POST",
        "/api/dashboards",
        &ctx.admin_token,
        Some(
            json!({
                "name": "sales",
                "label": "Sales overview",
                "items": [{"report_id": report_id.to_string(), "title": "All customers", "span": 6}]
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let dash_id = v["id"].as_str().unwrap().parse::<Uuid>().unwrap();

    let (st, v) = call(
        &ctx.app,
        "GET",
        &format!("/api/dashboards/{dash_id}"),
        &ctx.admin_token,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["label"], "Sales overview");
    let tile = &v["items"][0];
    assert_eq!(tile["title"], "All customers");
    assert_eq!(tile["report"]["name"], "customers");
    assert_eq!(tile["result"]["rows"].as_array().unwrap().len(), 2);
    assert_eq!(tile["result"]["rows"][0]["name"], "Acme");

    // a tile with a dangling report_id renders an error, not a 500
    let (st, v) = call(
        &ctx.app,
        "POST",
        "/api/dashboards",
        &ctx.admin_token,
        Some(
            json!({
                "name": "broken",
                "items": [{"report_id": Uuid::new_v4().to_string()}]
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let id = v["id"].as_str().unwrap().parse::<Uuid>().unwrap();
    let (st, v) = call(
        &ctx.app,
        "GET",
        &format!("/api/dashboards/{id}"),
        &ctx.admin_token,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(v["items"][0]["error"].is_string(), "{v}");

    // dashboards require a real name (no 'default' magic)
    let (st, _) = call(
        &ctx.app,
        "POST",
        "/api/dashboards",
        &ctx.admin_token,
        Some(json!({"name":"default","items":[]}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);

    // an empty-string report_id is rejected too — it would store a tile that
    // can never resolve (a bare `is_none()` check lets "" through)
    let (st, v) = call(
        &ctx.app,
        "POST",
        "/api/dashboards",
        &ctx.admin_token,
        Some(json!({"name":"empty-tile","items":[{"report_id":"","title":"x"}]}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
}

#[tokio::test]
async fn navigation_is_permission_filtered() {
    let Some(ctx) = setup().await else { return };
    publish(
        &ctx,
        crm_model(
            &format!("customer_{}", Uuid::new_v4().simple()),
            &format!("region_{}", Uuid::new_v4().simple()),
        ),
    )
    .await;

    // default: every readable entity
    let (st, v) = call(&ctx.app, "GET", "/api/navigation", &ctx.admin_token, None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let entities: Vec<&str> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["type"] == "entity")
        .map(|i| i["entity"].as_str().unwrap())
        .collect();
    assert!(entities.contains(&"Customer") && entities.contains(&"Region"));

    // a user who can only read Region sees just Region
    let hash = mda_security::hash_password("x").unwrap();
    let email = format!("u{}@test", Uuid::new_v4().simple());
    let uid = common::seed_user(&ctx.pool, ctx.tenant, &email, "u", &hash).await;
    let role = common::seed_role(&ctx.pool, ctx.tenant, "regional", &[("Region", "read")]).await;
    common::seed_assignment(&ctx.pool, ctx.tenant, uid, role).await;
    let token = JwtConfig::from_env()
        .issue_access(uid, ctx.tenant, None)
        .unwrap();
    let (_, v) = call(&ctx.app, "GET", "/api/navigation", &token, None).await;
    let entities: Vec<&str> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["entity"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(entities, vec!["Region"], "unreadable entities never appear");

    // authored navigation: order + labels + external link; non-http rejected
    let (st, _) = call(
        &ctx.app,
        "POST",
        "/api/navigation",
        &ctx.admin_token,
        Some(
            json!({"items": [
                {"type": "entity", "entity": "Region", "label": "Regions"},
                {"type": "link", "url": "https://example.com", "label": "Docs"}
            ]})
            .to_string(),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let (st, v) = call(&ctx.app, "GET", "/api/navigation", &ctx.admin_token, None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["items"][0]["label"], "Regions");
    assert_eq!(v["items"][1]["url"], "https://example.com");
    // the regional user sees the Region entry (entity items are permission
    // filtered) plus the global link — links are tenant nav chrome, not data.
    let (_, v) = call(&ctx.app, "GET", "/api/navigation", &token, None).await;
    let items = v["items"].as_array().unwrap().clone();
    assert_eq!(items.len(), 2, "{items:?}");
    assert_eq!(items[0]["entity"], "Region");
    assert_eq!(items[1]["url"], "https://example.com");

    let (st, _) = call(
        &ctx.app,
        "POST",
        "/api/navigation",
        &ctx.admin_token,
        Some(json!({"items": [{"type":"link","url":"file:///etc/passwd"}]}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "only http(s) links");

    // an entity item with an empty entity is rejected — it would be stored
    // and then silently vanish from every menu (never readable)
    let (st, v) = call(
        &ctx.app,
        "POST",
        "/api/navigation",
        &ctx.admin_token,
        Some(json!({"items": [{"type":"entity","entity":"","label":"ghost"}]}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{v}");

    let (st, _) = call(
        &ctx.app,
        "DELETE",
        "/api/navigation/default",
        &ctx.admin_token,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
}
