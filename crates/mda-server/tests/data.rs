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

struct Ctx {
    app: axum::Router,
    token: String,
    jwt: JwtConfig,
    pool: PgPool,
    tenant: Uuid,
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

async fn setup() -> Option<Ctx> {
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
    let tenant = Uuid::new_v4();
    let (role_id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO sec.sec_role (tenant_id, name) VALUES ($1, 'admin') RETURNING id",
    )
    .bind(tenant)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO sec.sec_permission (role_id, entity, verb) VALUES ($1, '*', '*')")
        .bind(role_id)
        .execute(&pool)
        .await
        .unwrap();
    let hash = mda_security::hash_password("x").unwrap();
    let email = format!("u{}@test", Uuid::new_v4().simple());
    let (user_id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO sec.sec_user (tenant_id, email, name, password_hash) VALUES ($1, $2, 'admin', $3) RETURNING id",
    )
    .bind(tenant)
    .bind(&email)
    .bind(&hash)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO sec.sec_role_assignment (user_id, role_id) VALUES ($1, $2)")
        .bind(user_id)
        .bind(role_id)
        .execute(&pool)
        .await
        .unwrap();
    let jwt = JwtConfig::from_env();
    let token = jwt.issue_access(user_id, tenant).unwrap();
    let app = mda_api::router(AppState {
        pool: pool.clone(),
        cache: MetadataCache::new(),
        jwt: jwt.clone(),
    });
    Some(Ctx {
        app,
        token,
        jwt,
        pool,
        tenant,
    })
}

/// Create a user with the given permissions; return a bearer token.
async fn limited_user(ctx: &Ctx, perms: &[(&str, &str)]) -> String {
    let (role_id,): (Uuid,) =
        sqlx::query_as("INSERT INTO sec.sec_role (tenant_id, name) VALUES ($1, $2) RETURNING id")
            .bind(ctx.tenant)
            .bind(format!("r{}", Uuid::new_v4().simple()))
            .fetch_one(&ctx.pool)
            .await
            .unwrap();
    for (e, v) in perms {
        sqlx::query("INSERT INTO sec.sec_permission (role_id, entity, verb) VALUES ($1, $2, $3)")
            .bind(role_id)
            .bind(e)
            .bind(v)
            .execute(&ctx.pool)
            .await
            .unwrap();
    }
    let hash = mda_security::hash_password("x").unwrap();
    let email = format!("u{}@test", Uuid::new_v4().simple());
    let (user_id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO sec.sec_user (tenant_id, email, name, password_hash) VALUES ($1, $2, 'limited', $3) RETURNING id",
    )
    .bind(ctx.tenant)
    .bind(&email)
    .bind(&hash)
    .fetch_one(&ctx.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO sec.sec_role_assignment (user_id, role_id) VALUES ($1, $2)")
        .bind(user_id)
        .bind(role_id)
        .execute(&ctx.pool)
        .await
        .unwrap();
    ctx.jwt.issue_access(user_id, ctx.tenant).unwrap()
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
    let none_token = limited_user(&ctx, &[]).await;
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
    let rw_token = limited_user(&ctx, &[("Customer", "read"), ("Customer", "create")]).await;
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
    let reader_token = limited_user(&ctx, &[("Customer", "read")]).await;
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
    sqlx::query(
        "INSERT INTO meta.md_rule (tenant_id, entity, event, condition, action_type, action_field, action_value)
         VALUES ($1,'Ticket','after_update',
            '{\"op\":\"Cmp\",\"kind\":\"eq\",\"lhs\":{\"op\":\"Field\",\"name\":\"status\"},\"rhs\":{\"op\":\"Lit\",\"value\":\"Closed\"}}'::jsonb,
            'set_field','closed_at','{\"op\":\"Call\",\"name\":\"now\",\"args\":[]}'::jsonb)",
    )
    .bind(ctx.tenant)
    .execute(&ctx.pool)
    .await
    .unwrap();

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

    // author the workflow via metadata (Studio is Phase 8)
    let (wf_id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO meta.md_workflow (tenant_id, entity, name) VALUES ($1,'Invoice','approval') RETURNING id",
    )
    .bind(ctx.tenant)
    .fetch_one(&ctx.pool)
    .await
    .unwrap();
    for s in ["active", "Submitted", "Approved"] {
        sqlx::query("INSERT INTO meta.md_workflow_state (workflow_id, name) VALUES ($1,$2)")
            .bind(wf_id)
            .bind(s)
            .execute(&ctx.pool)
            .await
            .unwrap();
    }
    // submit: active -> Submitted (creates an approval task)
    sqlx::query("INSERT INTO meta.md_workflow_transition (workflow_id, name, from_state, to_state, creates_task) VALUES ($1,'submit','active','Submitted',TRUE)")
        .bind(wf_id).execute(&ctx.pool).await.unwrap();
    // approve: Submitted -> Approved (guard amount>0; action approved_at=now())
    sqlx::query(
        "INSERT INTO meta.md_workflow_transition (workflow_id, name, from_state, to_state, guard, actions)
         VALUES ($1,'approve','Submitted','Approved',
            '{\"op\":\"Cmp\",\"kind\":\"gt\",\"lhs\":{\"op\":\"Field\",\"name\":\"amount\"},\"rhs\":{\"op\":\"Lit\",\"value\":0}}'::jsonb,
            '[{\"field\":\"approved_at\",\"value\":{\"op\":\"Call\",\"name\":\"now\",\"args\":[]}}]'::jsonb)",
    )
    .bind(wf_id).execute(&ctx.pool).await.unwrap();

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
    let tasks: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM meta.md_workflow_task WHERE tenant_id=$1 AND record_id=$2",
    )
    .bind(ctx.tenant)
    .bind(Uuid::parse_str(&id).unwrap())
    .fetch_one(&ctx.pool)
    .await
    .unwrap();
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
