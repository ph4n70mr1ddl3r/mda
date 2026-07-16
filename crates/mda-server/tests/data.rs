//! Phase 2 end-to-end: publish creates `biz.<table>` (with native FKs); CRUD via
//! `/api/data/:entity`; OCC `If-Match`; list/filter; add-field migration; and
//! two-phase retire. Runs in-process against the router; unique tenant per test.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use mda_api::AppState;
use mda_meta::MetadataCache;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;
use uuid::Uuid;

async fn app(pool: sqlx::PgPool) -> axum::Router {
    mda_api::router(AppState {
        pool,
        cache: MetadataCache::new(),
    })
}

async fn setup() -> Option<axum::Router> {
    let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| {
        eprintln!("skipping: DATABASE_URL not set");
        String::new()
    });
    if url.is_empty() {
        return None;
    }
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .unwrap();
    mda_server::migrate::run(&pool).await.unwrap();
    Some(app(pool).await)
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    tenant: Uuid,
    body: Option<String>,
    if_match: Option<String>,
) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(uri);
    b = b.header("x-tenant-id", tenant.to_string());
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

/// Branch → edit (full model) → publish; returns the publish result.
async fn publish(app: &axum::Router, tenant: Uuid, model: Value) -> Value {
    let (_, d) = call(
        app,
        "POST",
        "/api/studio/drafts",
        tenant,
        Some(json!({"name":"p"}).to_string()),
        None,
    )
    .await;
    let id = d["id"].as_str().unwrap().parse::<Uuid>().unwrap();
    let etag = d["version_etag"].as_str().unwrap();
    let _ = call(
        app,
        "PUT",
        &format!("/api/studio/drafts/{id}/model"),
        tenant,
        Some(model.to_string()),
        Some(etag.to_string()),
    )
    .await;
    let (_, res) = call(
        app,
        "POST",
        &format!("/api/studio/drafts/{id}/publish"),
        tenant,
        None,
        None,
    )
    .await;
    res
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

#[tokio::test]
async fn publish_creates_biz_table_and_crud_works() {
    let app = match setup().await {
        Some(a) => a,
        None => return,
    };
    let tenant = Uuid::new_v4();

    // 1. publish Customer -> biz.customer created
    publish(&app, tenant, customer_model()).await;

    // 2. create a record
    let (st, rec) = call(
        &app,
        "POST",
        "/api/data/Customer",
        tenant,
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

    // 3. read it back
    let (st, got) = call(
        &app,
        "GET",
        &format!("/api/data/Customer/{id}"),
        tenant,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(got["name"], "Acme");
    assert_eq!(got["email"], "acme@example.com");
    assert_eq!(got["tier"], "Gold");
    assert_eq!(got["balance"], 1234.5);

    // 4. OCC: wrong version -> 409; right version -> 200 + version bump
    let (st, _) = call(
        &app,
        "PATCH",
        &format!("/api/data/Customer/{id}"),
        tenant,
        Some(json!({"tier":"Silver"}).to_string()),
        Some(99.to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
    let (st, upd) = call(
        &app,
        "PATCH",
        &format!("/api/data/Customer/{id}"),
        tenant,
        Some(json!({"tier":"Silver"}).to_string()),
        Some(1.to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "update: {upd}");
    assert_eq!(upd["version"], 2);
    assert_eq!(upd["tier"], "Silver");

    // 5. list with filter
    let (st, list) = call(
        &app,
        "GET",
        "/api/data/Customer?filter=email:eq:acme%40example.com&sort=-created_at&page=1&page_size=10",
        tenant,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "list: {list}");
    assert_eq!(list["total"], 1);
    assert_eq!(list["items"][0]["email"], "acme@example.com");

    // 6. delete
    let (st, _) = call(
        &app,
        "DELETE",
        &format!("/api/data/Customer/{id}"),
        tenant,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (st, _) = call(
        &app,
        "GET",
        &format!("/api/data/Customer/{id}"),
        tenant,
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn native_fk_enforced_for_references() {
    let app = match setup().await {
        Some(a) => a,
        None => return,
    };
    let tenant = Uuid::new_v4();

    // publish Customer, then Order with a lookup -> Customer
    publish(&app, tenant, customer_model()).await;
    let (_, active) = call(&app, "GET", "/api/studio/model", tenant, None, None).await;
    let customer_id = active["entities"][0]["id"]
        .as_str()
        .unwrap()
        .parse::<Uuid>()
        .unwrap();

    let mut model = active.clone();
    model["entities"].as_array_mut().unwrap().push(json!({
        "id": Uuid::new_v4(),
        "module_id": null,
        "name": "Order",
        "table_name": format!("order_{}", Uuid::new_v4().simple()),
        "label": "Order",
        "description": null,
        "fields": [
            {"id": Uuid::new_v4(), "name":"amount","label":"Amount","field_type":"decimal","required":true,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{"precision":12,"scale":2}}
        ],
        "relationships": [{
            "id": Uuid::new_v4(),
            "source_field_name":"ref_customer_id",
            "target_entity_id": customer_id,
            "cardinality":"many_to_one",
            "strength":"lookup",
            "on_delete":"set_null",
            "required":false,
            "reference_qualifier":null,
            "rollup_summary":null
        }]
    }));
    publish(&app, tenant, model).await;

    // create an Order pointing at a real Customer -> ok
    let (st, _) = call(
        &app,
        "POST",
        "/api/data/Customer",
        tenant,
        Some(json!({"name":"Foo"}).to_string()),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let (_, cust) = call(
        &app,
        "GET",
        "/api/data/Customer?filter=name:eq:Foo",
        tenant,
        None,
        None,
    )
    .await;
    let cid = cust["items"][0]["id"].as_str().unwrap().to_string();

    let (st, _) = call(
        &app,
        "POST",
        "/api/data/Order",
        tenant,
        Some(json!({"amount":10.0,"ref_customer_id":cid}).to_string()),
        None,
    )
    .await;
    assert_eq!(
        st,
        StatusCode::CREATED,
        "order with valid FK should succeed"
    );

    // create an Order pointing at a nonexistent Customer -> native FK rejects
    let (st, _) = call(
        &app,
        "POST",
        "/api/data/Order",
        tenant,
        Some(json!({"amount":1.0,"ref_customer_id": Uuid::new_v4().to_string()}).to_string()),
        None,
    )
    .await;
    assert!(
        !st.is_success(),
        "order with dangling FK must be rejected (got {st})"
    );
}

#[tokio::test]
async fn add_field_migration_and_retire() {
    let app = match setup().await {
        Some(a) => a,
        None => return,
    };
    let tenant = Uuid::new_v4();

    // publish Customer with 2 fields
    let mut m = customer_model();
    // shrink to name+email for clarity
    m["entities"][0]["fields"] = json!([
        {"id": Uuid::new_v4(), "name":"name","label":"Name","field_type":"string","required":true,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{}},
        {"id": Uuid::new_v4(), "name":"email","label":"Email","field_type":"string","required":false,"is_unique":true,"is_indexed":false,"default_expr":null,"config":{}}
    ]);
    publish(&app, tenant, m.clone()).await;

    // additive: add an indexed field "phone"
    let mut with_phone = m.clone();
    with_phone["entities"][0]["fields"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id": Uuid::new_v4(), "name":"phone","label":"Phone","field_type":"string","required":false,"is_unique":false,"is_indexed":true,"default_expr":null,"config":{}}));
    let res = publish(&app, tenant, with_phone).await;
    assert_eq!(res["additions"]["fields"], 1, "add-field publish: {res}");

    // the new field is usable
    let (st, rec) = call(
        &app,
        "POST",
        "/api/data/Customer",
        tenant,
        Some(json!({"name":"Bar","email":"bar@example.com","phone":"555"}).to_string()),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "create with phone: {rec}");
    assert_eq!(rec["phone"], "555");

    // retire: publish a model with Customer removed -> two-phase retire
    let res = publish(&app, tenant, json!({"modules":[],"entities":[]})).await;
    assert_eq!(res["retirements"]["entities"], 1, "retire: {res}");

    // retired entity is no longer addressable at runtime
    let (st, _) = call(&app, "GET", "/api/data/Customer", tenant, None, None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}
