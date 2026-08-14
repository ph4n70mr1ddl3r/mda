//! Internationalization — metadata/UI string translations (PLAN §9 / Phase 11
//! deferral). Verifies CRUD, best-match locale resolution (exact → language
//! prefix → default), tenant isolation, and that the resolved bundle is injected
//! into the template render context (§5.19) so `{{ i18n.k }}` localizes.

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
    token: String,
    pool: PgPool,
    tenant: Uuid,
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

async fn upsert(ctx: &Ctx, locale: &str, ns: &str, key: &str, value: &str) {
    let (st, _) = call(
        &ctx.app,
        "POST",
        "/api/translations",
        &ctx.token,
        Some(json!({"locale":locale,"namespace":ns,"key":key,"value":value}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "upsert {locale}/{ns}.{key}");
}

#[tokio::test]
async fn upsert_list_and_delete_translations() {
    let Some(ctx) = setup().await else {
        return;
    };
    upsert(&ctx, "", "ui", "greeting", "Hello").await;
    upsert(&ctx, "fr", "ui", "greeting", "Bonjour").await;
    // upsert updates in place (same natural key → no duplicate).
    upsert(&ctx, "fr", "ui", "greeting", "Salut").await;

    let (st, list) = call(&ctx.app, "GET", "/api/translations", &ctx.token, None).await;
    assert_eq!(st, StatusCode::OK);
    // two distinct (locale,key) rows after the update-in-place.
    assert_eq!(list.as_array().unwrap().len(), 2, "{list}");
    let fr = list
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["locale"] == "fr")
        .unwrap();
    assert_eq!(fr["value"], "Salut", "upsert updated the value");

    // delete one key
    let (st, _) = call(
        &ctx.app,
        "DELETE",
        "/api/translations/fr/ui/greeting",
        &ctx.token,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (_, list) = call(&ctx.app, "GET", "/api/translations", &ctx.token, None).await;
    assert_eq!(list.as_array().unwrap().len(), 1);

    // deleting a missing key → 404
    let (st, _) = call(
        &ctx.app,
        "DELETE",
        "/api/translations/fr/ui/greeting",
        &ctx.token,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn best_match_locale_resolution() {
    let Some(ctx) = setup().await else {
        return;
    };
    // default bundle + a partial fr translation (only greeting, not farewell).
    upsert(&ctx, "", "ui", "greeting", "Hello").await;
    upsert(&ctx, "", "ui", "farewell", "Goodbye").await;
    upsert(&ctx, "fr", "ui", "greeting", "Bonjour").await;
    // (no fr farewell → must fall back to the default.)

    // exact fr → greeting localized, farewell falls back to default.
    let (st, bundle) = call(&ctx.app, "GET", "/api/i18n/fr", &ctx.token, None).await;
    assert_eq!(st, StatusCode::OK, "{bundle}");
    let t = &bundle["translations"];
    assert_eq!(t["ui.greeting"], "Bonjour", "exact locale wins");
    assert_eq!(
        t["ui.farewell"], "Goodbye",
        "missing key falls back to default"
    );

    // fr-CA → language prefix fr wins for greeting; default for farewell.
    let (_, bundle) = call(&ctx.app, "GET", "/api/i18n/fr-CA", &ctx.token, None).await;
    let t = &bundle["translations"];
    assert_eq!(t["ui.greeting"], "Bonjour", "language prefix (fr) match");
    assert_eq!(t["ui.farewell"], "Goodbye");

    // de (no translations at all) → full default fallback.
    let (_, bundle) = call(&ctx.app, "GET", "/api/i18n/de", &ctx.token, None).await;
    let t = &bundle["translations"];
    assert_eq!(t["ui.greeting"], "Hello");
    assert_eq!(t["ui.farewell"], "Goodbye");

    // namespace scoping
    let (st, bundle) = call(
        &ctx.app,
        "GET",
        "/api/i18n/fr?namespace=ui",
        &ctx.token,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(bundle["translations"]["ui.greeting"], "Bonjour");
}

#[tokio::test]
async fn translations_are_tenant_isolated() {
    let Some(ctx) = setup().await else {
        return;
    };
    upsert(&ctx, "", "ui", "greeting", "Hello-A").await;

    // A second tenant with its own admin.
    let tenant_b = Uuid::new_v4();
    let role_b = common::seed_role(&ctx.pool, tenant_b, "admin", &[("*", "*")]).await;
    let hash = mda_security::hash_password("x").unwrap();
    let email = format!("b{}@test", Uuid::new_v4().simple());
    let user_b = common::seed_user(&ctx.pool, tenant_b, &email, "b", &hash).await;
    common::seed_assignment(&ctx.pool, tenant_b, user_b, role_b).await;
    let token_b = JwtConfig::from_env()
        .issue_access(user_b, tenant_b, None)
        .unwrap();

    // Tenant B sees NONE of tenant A's translations.
    let (_, list_b) = call(&ctx.app, "GET", "/api/translations", &token_b, None).await;
    assert_eq!(
        list_b.as_array().unwrap().len(),
        0,
        "tenant B sees nothing of A"
    );

    // Tenant B writes its own; A is unaffected.
    let _ = call(
        &ctx.app,
        "POST",
        "/api/translations",
        &token_b,
        Some(json!({"locale":"","namespace":"ui","key":"greeting","value":"Hello-B"}).to_string()),
    )
    .await;
    let (_, bundle_a) = call(&ctx.app, "GET", "/api/i18n/en", &ctx.token, None).await;
    assert_eq!(
        bundle_a["translations"]["ui.greeting"], "Hello-A",
        "tenant A's value is its own: {bundle_a}"
    );
    let (_, bundle_b) = call(&ctx.app, "GET", "/api/i18n/en", &token_b, None).await;
    assert_eq!(bundle_b["translations"]["ui.greeting"], "Hello-B");
}

#[tokio::test]
async fn template_render_localizes_via_i18n_bundle() {
    let Some(ctx) = setup().await else {
        return;
    };
    // default + fr translations for the template's strings.
    upsert(&ctx, "", "email", "subject", "Your order").await;
    upsert(&ctx, "fr", "email", "subject", "Votre commande").await;

    // author a template that interpolates the localized bundle.
    let mut tx = ctx.pool.begin().await.unwrap();
    mda_security::set_tenant(&mut tx, ctx.tenant).await.unwrap();
    sqlx::query(
        "INSERT INTO meta.md_template (tenant_id, name, kind, body, content_type, locale)
         VALUES ($1,'order','message','Subject: {{ i18n.email.subject }}','text/plain',NULL)
         ON CONFLICT (tenant_id, name, locale) DO UPDATE SET body = EXCLUDED.body",
    )
    .bind(ctx.tenant)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();

    // render in fr → localized subject.
    let (st, res) = call(
        &ctx.app,
        "POST",
        "/api/templates/order/render?locale=fr",
        &ctx.token,
        Some(json!({}).to_string()),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{res}");
    assert_eq!(
        res["body"], "Subject: Votre commande",
        "render localized: {res}"
    );

    // render in default (no locale) → default bundle.
    let (_, res) = call(
        &ctx.app,
        "POST",
        "/api/templates/order/render",
        &ctx.token,
        Some(json!({}).to_string()),
    )
    .await;
    assert_eq!(res["body"], "Subject: Your order");
}
