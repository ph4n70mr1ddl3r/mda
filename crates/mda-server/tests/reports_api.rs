//! Reporting API completion (Phase 7): report authoring CRUD, reference
//! traversal (`customer.name` via a real LEFT JOIN over the hoisted FK), the
//! CSV/HTML/XLSX/PDF export renderers, the full record-scope predicate in
//! report runs, and scheduled-report notification delivery (`config.notify`).

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
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

fn order_model(order_table: &str, customer_table: &str) -> Value {
    let customer_id = Uuid::new_v4();
    let order_id = Uuid::new_v4();
    json!({
        "modules": [],
        "entities": [
            {
                "id": customer_id, "module_id": null, "name": "Customer",
                "table_name": customer_table, "label": "Customer", "description": null,
                "fields": [
                    {"id": Uuid::new_v4(), "name":"name","label":"Name","field_type":"string","required":true,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{}}
                ],
                "relationships": []
            },
            {
                "id": order_id, "module_id": null, "name": "Order",
                "table_name": order_table, "label": "Order", "description": null,
                "fields": [
                    {"id": Uuid::new_v4(), "name":"ref","label":"Ref","field_type":"string","required":true,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{}},
                    {"id": Uuid::new_v4(), "name":"total","label":"Total","field_type":"decimal","required":false,"is_unique":false,"is_indexed":false,"default_expr":null,"config":{}}
                ],
                "relationships": [
                    {"id": Uuid::new_v4(), "source_entity_id": order_id, "source_field_name":"customer_id",
                     "target_entity_id": customer_id, "cardinality":"many_to_one", "strength":"lookup",
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

async fn raw_get(app: &axum::Router, uri: &str, token: &str) -> (StatusCode, Vec<u8>, String) {
    let req = Request::builder()
        .uri(uri)
        .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let ct = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, bytes, ct)
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

async fn seed_data(ctx: &Ctx) -> Uuid {
    let (st, v) = call(
        &ctx.app,
        "POST",
        "/api/data/Customer",
        &ctx.admin_token,
        Some(json!({"name":"Acme"}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let acme = v["id"].as_str().unwrap().parse::<Uuid>().unwrap();
    call(
        &ctx.app,
        "POST",
        "/api/data/Customer",
        &ctx.admin_token,
        Some(json!({"name":"Zeta"}).to_string()),
    )
    .await;
    for (r, total, cust) in [
        ("R-1", 100, Some(acme)),
        ("R-2", 250, Some(acme)),
        ("R-3", 50, None),
    ] {
        let mut body = json!({"ref": r, "total": total});
        if let Some(c) = cust {
            body["customer_id"] = json!(c.to_string());
        }
        let (st, v) = call(
            &ctx.app,
            "POST",
            "/api/data/Order",
            &ctx.admin_token,
            Some(body.to_string()),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "{v}");
    }
    acme
}

#[tokio::test]
async fn report_crud_and_runs() {
    let Some(ctx) = setup().await else { return };
    publish(
        &ctx,
        order_model(
            &format!("order_{}", Uuid::new_v4().simple()),
            &format!("customer_{}", Uuid::new_v4().simple()),
        ),
    )
    .await;
    seed_data(&ctx).await;

    // author-time validation: unknown base entity rejected
    let (st, _) = call(
        &ctx.app,
        "POST",
        "/api/reports",
        &ctx.admin_token,
        Some(
            json!({"name":"x","dataset":{"base_entity":"Nope","fields":[{"field":"a"}]}})
                .to_string(),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);

    let (st, v) = call(
        &ctx.app,
        "POST",
        "/api/reports",
        &ctx.admin_token,
        Some(
            json!({
                "name": "order-totals",
                "dataset": {
                    "base_entity":"Order",
                    "fields":[{"field":"*","aggregate":"count","alias":"n"},{"field":"total","aggregate":"sum","alias":"sum_total"}]
                }
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let id = v["id"].as_str().unwrap().parse::<Uuid>().unwrap();

    let (st, list) = call(&ctx.app, "GET", "/api/reports", &ctx.admin_token, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 1);

    let (st, run) = call(
        &ctx.app,
        "GET",
        &format!("/api/reports/{id}/run"),
        &ctx.admin_token,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{run}");
    assert_eq!(run["rows"][0]["n"], 3);
    assert_eq!(run["rows"][0]["sum_total"], 400.0);

    // PATCH renames; DELETE removes
    let (st, v) = call(
        &ctx.app,
        "PATCH",
        &format!("/api/reports/{id}"),
        &ctx.admin_token,
        Some(json!({"name":"totals"}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["name"], "totals");
    let (st, _) = call(
        &ctx.app,
        "DELETE",
        &format!("/api/reports/{id}"),
        &ctx.admin_token,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (st, _) = call(
        &ctx.app,
        "GET",
        &format!("/api/reports/{id}"),
        &ctx.admin_token,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn reference_traversal_joins_the_target_entity() {
    let Some(ctx) = setup().await else { return };
    publish(
        &ctx,
        order_model(
            &format!("order_{}", Uuid::new_v4().simple()),
            &format!("customer_{}", Uuid::new_v4().simple()),
        ),
    )
    .await;
    let acme = seed_data(&ctx).await;

    // select the customer's NAME through the order (customer.name), and filter
    // on it — both compile to LEFT JOINs over the hoisted FK column
    let dataset = json!({
        "base_entity":"Order",
        "fields":[{"field":"ref"},{"field":"customer_id.name","alias":"customer"}],
        "filters":[{"field":"customer_id.name","op":"eq","value":"Acme"}],
        "order_by":[{"field":"ref","asc":true}]
    });
    let res = mda_reports::run(
        &ctx.pool,
        &identity(&ctx).await,
        &serde_json::from_value(dataset).unwrap(),
    )
    .await
    .expect("run");
    assert_eq!(res.rows.len(), 2, "only Acme orders: {:?}", res.rows);
    assert_eq!(res.rows[0]["customer"].as_str(), Some("Acme"));
    assert_eq!(res.rows[0]["ref"].as_str(), Some("R-1"));

    // filter by the FK itself (customer_id eq acme)
    let dataset = json!({
        "base_entity":"Order",
        "fields":[{"field":"ref"}],
        "filters":[{"field":"customer_id","op":"eq","value": acme.to_string()}]
    });
    let res = mda_reports::run(
        &ctx.pool,
        &identity(&ctx).await,
        &serde_json::from_value(dataset).unwrap(),
    )
    .await
    .expect("run");
    assert_eq!(res.rows.len(), 2);

    // an unknown field path is a clean 4xx-shaped error
    let dataset = json!({
        "base_entity":"Order",
        "fields":[{"field":"customer.nope","alias":"x"}]
    });
    let err = mda_reports::run(
        &ctx.pool,
        &identity(&ctx).await,
        &serde_json::from_value(dataset).unwrap(),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("unknown field"), "{err}");
}

async fn identity(ctx: &Ctx) -> mda_security::Identity {
    mda_security::load_identity(&ctx.pool, ctx.admin_id, ctx.tenant)
        .await
        .expect("load identity")
}

/// A dataset selecting the same alias twice must 422, not silently shadow:
/// jsonb_build_object keeps the *last* pair for a repeated key, so the first
/// field's values would vanish from every row (and the CSV export would carry
/// a duplicate header that the impex import parser rejects).
#[tokio::test]
async fn duplicate_select_aliases_are_rejected_not_shadowed() {
    let Some(ctx) = setup().await else { return };
    publish(
        &ctx,
        order_model(
            &format!("order_{}", Uuid::new_v4().simple()),
            &format!("customer_{}", Uuid::new_v4().simple()),
        ),
    )
    .await;

    let (st, v) = call(
        &ctx.app,
        "POST",
        "/api/reports",
        &ctx.admin_token,
        Some(
            json!({
                "name": "dup-alias",
                "dataset": {
                    "base_entity":"Order",
                    "fields":[{"field":"ref","alias":"a"},{"field":"total","alias":"a"}]
                }
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let id = v["id"].as_str().unwrap().parse::<Uuid>().unwrap();

    let (st, run) = call(
        &ctx.app,
        "GET",
        &format!("/api/reports/{id}/run"),
        &ctx.admin_token,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{run}");
    assert!(
        run["message"]
            .as_str()
            .unwrap_or("")
            .contains("duplicate select alias"),
        "{run}"
    );
}

#[tokio::test]
async fn export_renders_csv_html_xlsx_and_pdf() {
    let Some(ctx) = setup().await else { return };
    publish(
        &ctx,
        order_model(
            &format!("order_{}", Uuid::new_v4().simple()),
            &format!("customer_{}", Uuid::new_v4().simple()),
        ),
    )
    .await;
    seed_data(&ctx).await;
    let (st, v) = call(
        &ctx.app,
        "POST",
        "/api/reports",
        &ctx.admin_token,
        Some(
            json!({
                "name": "orders",
                "dataset": {"base_entity":"Order","fields":[{"field":"ref"},{"field":"total"}],"order_by":[{"field":"ref","asc":true}]}
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let id = v["id"].as_str().unwrap().parse::<Uuid>().unwrap();

    let (st, body, ct) = raw_get(
        &ctx.app,
        &format!("/api/reports/{id}/export"),
        &ctx.admin_token,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(ct.starts_with("text/csv"), "{ct}");
    let csv = String::from_utf8(body).unwrap();
    assert!(csv.starts_with("ref,total"), "{csv}");

    let (st, body, ct) = raw_get(
        &ctx.app,
        &format!("/api/reports/{id}/export?format=html"),
        &ctx.admin_token,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(ct.starts_with("text/html"));
    assert!(String::from_utf8_lossy(&body).contains("<td>R-1</td>"));

    let (st, body, ct) = raw_get(
        &ctx.app,
        &format!("/api/reports/{id}/export?format=xlsx"),
        &ctx.admin_token,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{ct}");
    assert!(ct.starts_with("application/vnd.openxmlformats"), "{ct}");
    assert_eq!(&body[..2], b"PK", "xlsx is a zip");

    let (st, body, ct) = raw_get(
        &ctx.app,
        &format!("/api/reports/{id}/export?format=pdf"),
        &ctx.admin_token,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(ct, "application/pdf");
    assert!(body.starts_with(b"%PDF-1.4"));
    assert!(body.ends_with(b"%%EOF\n".as_slice()));

    let (st, _, _) = raw_get(
        &ctx.app,
        &format!("/api/reports/{id}/export?format=gif"),
        &ctx.admin_token,
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn report_runs_under_record_scope_not_just_owner() {
    let Some(ctx) = setup().await else { return };
    // fixed table names (this test owns its database) so the share can address
    // the row by table directly
    publish(
        &ctx,
        order_model(
            "order_rscope",
            &format!("customer_{}", Uuid::new_v4().simple()),
        ),
    )
    .await;
    seed_data(&ctx).await;

    let hash = mda_security::hash_password("x").unwrap();
    let email = format!("u{}@test", Uuid::new_v4().simple());
    let reader = common::seed_user(&ctx.pool, ctx.tenant, &email, "u", &hash).await;
    let role = common::seed_role(&ctx.pool, ctx.tenant, "orderviewer", &[("Order", "read")]).await;
    common::seed_assignment(&ctx.pool, ctx.tenant, reader, role).await;

    let mut tx = ctx.pool.begin().await.unwrap();
    mda_security::set_tenant(&mut tx, ctx.tenant).await.unwrap();
    let (shared_id,): (Uuid,) = sqlx::query_as(
        "SELECT id FROM biz.order_rscope WHERE tenant_id = $1 AND attributes->>'ref' = 'R-2'",
    )
    .bind(ctx.tenant)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sec.sec_record_share (tenant_id, entity, record_id, principal_id, access) \
         VALUES ($1, 'Order', $2, $3, 'read')",
    )
    .bind(ctx.tenant)
    .bind(shared_id)
    .bind(reader)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let ident = mda_security::load_identity(&ctx.pool, reader, ctx.tenant)
        .await
        .expect("load reader identity");
    let ds: mda_reports::Dataset = serde_json::from_value(json!({
        "base_entity":"Order","fields":[{"field":"ref"}],"order_by":[{"field":"ref","asc":true}]
    }))
    .unwrap();
    let res = mda_reports::run(&ctx.pool, &ident, &ds).await.expect("run");
    assert_eq!(res.rows.len(), 1, "shared record only: {:?}", res.rows);
    assert_eq!(res.rows[0]["ref"].as_str(), Some("R-2"));
}

#[tokio::test]
async fn scheduled_report_notifies_on_delivery() {
    let Some(ctx) = setup().await else { return };
    publish(
        &ctx,
        order_model(
            &format!("order_{}", Uuid::new_v4().simple()),
            &format!("customer_{}", Uuid::new_v4().simple()),
        ),
    )
    .await;
    seed_data(&ctx).await;
    let (st, v) = call(
        &ctx.app,
        "POST",
        "/api/reports",
        &ctx.admin_token,
        Some(
            json!({
                "name": "daily-orders",
                "dataset": {"base_entity":"Order","fields":[{"field":"*","aggregate":"count","alias":"n"}]}
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let report_id = v["id"].as_str().unwrap().parse::<Uuid>().unwrap();

    // schedule it with config.notify — a report.completed notification is
    // enqueued for the running user on every run
    let (st, v) = call(
        &ctx.app,
        "POST",
        "/api/schedules",
        &ctx.admin_token,
        Some(
            json!({
                "name":"daily","kind":"report","target_id":report_id,
                "cron":"0 0 6 * * *","config":{"notify": true}
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    assert_eq!(v["config"]["notify"], true, "config persisted: {v}");
    let sched_id = v["id"].as_str().unwrap().parse::<Uuid>().unwrap();

    let (st, _) = call(
        &ctx.app,
        "POST",
        &format!("/api/schedules/{sched_id}/run"),
        &ctx.admin_token,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sys_outbox WHERE tenant_id = $1 AND kind = 'notification.fanout' \
         AND payload->>'type_key' = 'report.completed'",
    )
    .bind(ctx.tenant)
    .fetch_one(&ctx.pool)
    .await
    .unwrap();
    assert_eq!(n, 1, "scheduled report delivery enqueued a notification");
}
