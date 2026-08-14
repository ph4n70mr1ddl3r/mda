//! Admin security-graph API (PLAN §5.11): a superuser-only management surface
//! for teams (incl. the ADR-0013 `parent_id` hierarchy), roles, object/field
//! permissions, org-wide defaults, role assignments, and users. Verifies the
//! whole CRUD surface + the cycle guard + that a non-admin is denied.

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

struct Ctx {
    app: axum::Router,
    pool: PgPool,
    tenant: Uuid,
    token: String,
    jwt: JwtConfig,
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
        pool,
        tenant,
        token,
        jwt,
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

/// A non-admin token (Customer read only).
async fn reader_token(ctx: &Ctx) -> String {
    let role_id = common::seed_role(&ctx.pool, ctx.tenant, "reader", &[("Customer", "read")]).await;
    let hash = mda_security::hash_password("x").unwrap();
    let email = format!("r{}@test", Uuid::new_v4().simple());
    let uid = common::seed_user(&ctx.pool, ctx.tenant, &email, "r", &hash).await;
    common::seed_assignment(&ctx.pool, ctx.tenant, uid, role_id).await;
    ctx.jwt.issue_access(uid, ctx.tenant, None).unwrap()
}

#[tokio::test]
async fn teams_crud_and_hierarchy() {
    let Some(ctx) = setup().await else {
        return;
    };

    // create a parent team, then a child team under it.
    let (st, parent) = call(
        &ctx.app,
        "POST",
        "/api/admin/teams",
        &ctx.token,
        Some(json!({"name":"Eng"}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let parent_id = parent["id"].as_str().unwrap().parse::<Uuid>().unwrap();
    assert_eq!(parent["parent_id"], Value::Null);

    let (st, child) = call(
        &ctx.app,
        "POST",
        "/api/admin/teams",
        &ctx.token,
        Some(json!({"name":"Eng-Platform","parent_id":parent_id}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let child_id = child["id"].as_str().unwrap().parse::<Uuid>().unwrap();
    assert_eq!(child["parent_id"], parent_id.to_string());

    // duplicate name → conflict
    let (st, _) = call(
        &ctx.app,
        "POST",
        "/api/admin/teams",
        &ctx.token,
        Some(json!({"name":"Eng"}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);

    // cycle guard: re-parenting Eng under Eng-Platform would create a cycle.
    let (st, body) = call(
        &ctx.app,
        "PATCH",
        &format!("/api/admin/teams/{parent_id}"),
        &ctx.token,
        Some(json!({"parent_id": child_id}).to_string()),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::UNPROCESSABLE_ENTITY,
        "cycle rejected: {body}"
    );
    assert_eq!(body["code"], "mda.invalid");

    // self-loop rejected.
    let (st, _) = call(
        &ctx.app,
        "PATCH",
        &format!("/api/admin/teams/{parent_id}"),
        &ctx.token,
        Some(json!({"parent_id": parent_id}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);

    // rename works.
    let (st, row) = call(
        &ctx.app,
        "PATCH",
        &format!("/api/admin/teams/{child_id}"),
        &ctx.token,
        Some(json!({"name":"Eng-Core"}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(row["name"], "Eng-Core");
    assert_eq!(row["parent_id"], parent_id.to_string());

    // list sees both.
    let (st, list) = call(&ctx.app, "GET", "/api/admin/teams", &ctx.token, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 2);

    // detach (root) the child via parent_id: null.
    let (st, row) = call(
        &ctx.app,
        "PATCH",
        &format!("/api/admin/teams/{child_id}"),
        &ctx.token,
        Some(json!({"parent_id": null}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(row["parent_id"], Value::Null);

    // delete the parent.
    let (st, _) = call(
        &ctx.app,
        "DELETE",
        &format!("/api/admin/teams/{parent_id}"),
        &ctx.token,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    // 404 on a missing team.
    let (st, _) = call(
        &ctx.app,
        "DELETE",
        &format!("/api/admin/teams/{parent_id}"),
        &ctx.token,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // a non-admin is denied everywhere on the surface.
    let rtok = reader_token(&ctx).await;
    let (st, body) = call(&ctx.app, "GET", "/api/admin/teams", &rtok, None).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "mda.forbidden");
}

#[tokio::test]
async fn roles_permissions_and_field_permissions() {
    let Some(ctx) = setup().await else {
        return;
    };
    let (st, role) = call(
        &ctx.app,
        "POST",
        "/api/admin/roles",
        &ctx.token,
        Some(json!({"name":"Editor"}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let role_id = role["id"].as_str().unwrap().parse::<Uuid>().unwrap();

    // grant object permission
    let (st, _) = call(
        &ctx.app,
        "POST",
        &format!("/api/admin/roles/{role_id}/permissions"),
        &ctx.token,
        Some(json!({"entity":"Customer","verb":"read"}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);

    // grant field permission
    let (st, _) = call(
        &ctx.app,
        "POST",
        &format!("/api/admin/roles/{role_id}/field-permissions"),
        &ctx.token,
        Some(json!({"entity":"Customer","field":"secret","access":"none"}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);

    // invalid access rejected
    let (st, body) = call(
        &ctx.app,
        "POST",
        &format!("/api/admin/roles/{role_id}/field-permissions"),
        &ctx.token,
        Some(json!({"entity":"Customer","field":"secret","access":"bogus"}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["code"], "mda.invalid");

    // list roles shows the permission + field-permission + zero users
    let (st, list) = call(&ctx.app, "GET", "/api/admin/roles", &ctx.token, None).await;
    assert_eq!(st, StatusCode::OK);
    let editor = list
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "Editor")
        .unwrap();
    assert_eq!(editor["user_count"], 0);
    assert_eq!(
        editor["permissions"],
        json!([{"entity":"Customer","verb":"read"}])
    );
    assert_eq!(
        editor["field_permissions"],
        json!([{"entity":"Customer","field":"secret","access":"none"}])
    );

    // revoke the field permission
    let (st, _) = call(
        &ctx.app,
        "DELETE",
        &format!("/api/admin/roles/{role_id}/field-permissions/Customer/secret"),
        &ctx.token,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (_st, list) = call(&ctx.app, "GET", "/api/admin/roles", &ctx.token, None).await;
    let editor = list
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "Editor")
        .unwrap();
    assert!(editor["field_permissions"].as_array().unwrap().is_empty());

    // delete the role cascades.
    let (st, _) = call(
        &ctx.app,
        "DELETE",
        &format!("/api/admin/roles/{role_id}"),
        &ctx.token,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn owd_set_and_get() {
    let Some(ctx) = setup().await else {
        return;
    };
    let (st, row) = call(
        &ctx.app,
        "PUT",
        "/api/admin/owd/Customer",
        &ctx.token,
        Some(json!({"default_access":"team"}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(row["entity"], "Customer");
    assert_eq!(row["default_access"], "team");

    // invalid value rejected
    let (st, body) = call(
        &ctx.app,
        "PUT",
        "/api/admin/owd/Customer",
        &ctx.token,
        Some(json!({"default_access":"wide_open"}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["code"], "mda.invalid");

    // upsert flips to private
    let (st, row) = call(
        &ctx.app,
        "PUT",
        "/api/admin/owd/Customer",
        &ctx.token,
        Some(json!({"default_access":"private"}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(row["default_access"], "private");

    let (st, list) = call(&ctx.app, "GET", "/api/admin/owd", &ctx.token, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn users_crud_assignments_and_password_reset() {
    let Some(ctx) = setup().await else {
        return;
    };
    // make a team to assign the user into
    let (_, team) = call(
        &ctx.app,
        "POST",
        "/api/admin/teams",
        &ctx.token,
        Some(json!({"name":"Sales"}).to_string()),
    )
    .await;
    let team_id = team["id"].as_str().unwrap().parse::<Uuid>().unwrap();

    let (st, user) = call(
        &ctx.app,
        "POST",
        "/api/admin/users",
        &ctx.token,
        Some(
            json!({"email":"alice@mda.local","name":"Alice","password":"secret123","team_id":team_id})
                .to_string(),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let uid = user["id"].as_str().unwrap().parse::<Uuid>().unwrap();
    assert_eq!(user["team_id"], team_id.to_string());
    assert_eq!(user["active"], true);
    // no password hash leaks
    assert!(user.get("password_hash").is_none());

    // duplicate email → conflict
    let (st, _) = call(
        &ctx.app,
        "POST",
        "/api/admin/users",
        &ctx.token,
        Some(json!({"email":"alice@mda.local","password":"secret123"}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);

    // create a role and assign it
    let (_, role) = call(
        &ctx.app,
        "POST",
        "/api/admin/roles",
        &ctx.token,
        Some(json!({"name":"Rep"}).to_string()),
    )
    .await;
    let role_id = role["id"].as_str().unwrap().parse::<Uuid>().unwrap();
    let (st, _) = call(
        &ctx.app,
        "POST",
        &format!("/api/admin/users/{uid}/roles"),
        &ctx.token,
        Some(json!({"role_id": role_id}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);

    // list user roles shows Rep
    let (st, roles) = call(
        &ctx.app,
        "GET",
        &format!("/api/admin/users/{uid}/roles"),
        &ctx.token,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(roles.as_array().unwrap().len(), 1);
    assert_eq!(roles[0]["name"], "Rep");

    // assign to a non-existent user → 404
    let (st, _) = call(
        &ctx.app,
        "POST",
        &format!("/api/admin/users/{}/roles", Uuid::new_v4()),
        &ctx.token,
        Some(json!({"role_id": role_id}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // password reset + can then log in with the new password
    let (st, _) = call(
        &ctx.app,
        "POST",
        &format!("/api/admin/users/{uid}/password"),
        &ctx.token,
        Some(json!({"password":"newpass456"}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);

    let (st, login) = call(
        &ctx.app,
        "POST",
        "/api/auth/login",
        &ctx.token,
        Some(
            json!({"tenant": ctx.tenant, "email":"alice@mda.local","password":"newpass456"})
                .to_string(),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "new password works: {login}");
    assert!(login["access_token"].as_str().is_some());

    // deactivate
    let (st, user) = call(
        &ctx.app,
        "PATCH",
        &format!("/api/admin/users/{uid}"),
        &ctx.token,
        Some(json!({"active": false}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(user["active"], false);

    // a deactivated user can no longer log in
    let (st, _) = call(
        &ctx.app,
        "POST",
        "/api/auth/login",
        &ctx.token,
        Some(
            json!({"tenant": ctx.tenant, "email":"alice@mda.local","password":"newpass456"})
                .to_string(),
        ),
    )
    .await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
}
