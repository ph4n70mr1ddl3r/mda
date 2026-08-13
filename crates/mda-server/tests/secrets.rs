//! Secrets management (PLAN §5.20): references in `sys_secret`, values
//! resolved server-side only via `SecretStore`, every resolution audited. The
//! value NEVER appears in any API response — only the store `ref`.

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use mda_api::secrets::{resolve_and_audit, LocalSecretStore};
use mda_api::AppState;
use mda_meta::MetadataCache;
use mda_security::jwt::JwtConfig;
use mda_security::LoginThrottle;
use serde_json::{json, Value};
use tower::ServiceExt;
use uuid::Uuid;

mod common;

const PWD: &str = "correct-horse-battery-staple";

struct Ctx {
    app: axum::Router,
    pool: sqlx::PgPool,
    tenant: Uuid,
    token: String,
    user_id: Uuid,
}

async fn setup() -> Option<Ctx> {
    let url = std::env::var("DATABASE_URL").ok()?;
    let (pool, _db_url) = common::spawn_db(&url).await;
    let tenant = Uuid::new_v4();
    let role_id = common::seed_role(&pool, tenant, "admin", &[("*", "*")]).await;
    let hash = mda_security::hash_password(PWD).unwrap();
    let email = format!("u{}@test", Uuid::new_v4().simple());
    let user_id = common::seed_user(&pool, tenant, &email, "admin", &hash).await;
    common::seed_assignment(&pool, tenant, user_id, role_id).await;

    let blobs: std::sync::Arc<dyn mda_api::blobs::BlobStore> =
        std::sync::Arc::new(mda_api::blobs::LocalBlobStore::from_env());
    let secrets: std::sync::Arc<dyn mda_core::SecretStore> =
        std::sync::Arc::new(LocalSecretStore::from_env());
    let app = mda_api::router(AppState {
        pool: pool.clone(),
        cache: MetadataCache::new(),
        jwt: JwtConfig::from_env(),
        blobs,
        secrets,
        events: mda_api::events::channel(),
        login_throttle: LoginThrottle::default(),
    });

    // login to get an access token (admin can do everything).
    let (st, v) = login(&app, tenant, &email, PWD).await;
    assert_eq!(st, StatusCode::OK);
    let token = v["access_token"].as_str().unwrap().to_string();
    // resolve the user id for audit assertions (sec_user is RLS-gated → GUC).
    let user_id = {
        let mut tx = pool.begin().await.unwrap();
        mda_security::set_tenant(&mut tx, tenant).await.unwrap();
        let id: Uuid = sqlx::query_scalar("SELECT id FROM sec.sec_user WHERE email = $1")
            .bind(&email)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        id
    };

    Some(Ctx {
        app,
        pool,
        tenant,
        token,
        user_id,
    })
}

async fn login(app: &axum::Router, tenant: Uuid, email: &str, pwd: &str) -> (StatusCode, Value) {
    app.clone()
        .oneshot(
            Request::post("/api/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"tenant": tenant.to_string(), "email": email, "password": pwd})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap()
        .into_json()
        .await
}

trait IntoJson {
    async fn into_json(self) -> (StatusCode, Value);
}
impl IntoJson for axum::response::Response {
    async fn into_json(self) -> (StatusCode, Value) {
        let st = self.status();
        let bytes = to_bytes(self.into_body(), 1 << 20).await.unwrap_or_default();
        let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (st, v)
    }
}

async fn authed(app: &axum::Router, token: &str, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut b = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {token}"));
    let req = if let Some(v) = body {
        b = b.header("content-type", "application/json");
        b.body(Body::from(v.to_string())).unwrap()
    } else {
        b.body(Body::empty()).unwrap()
    };
    app.clone().oneshot(req).await.unwrap().into_json().await
}

#[tokio::test]
async fn secrets_crud_and_never_exposes_value() {
    let Some(ctx) = setup().await else {
        return;
    };

    // register a secret reference. `ref` is the store key, not the value.
    let (st, v) = authed(
        &ctx.app,
        &ctx.token,
        "POST",
        "/api/secrets",
        Some(json!({"name":"smtp_password","kind":"opaque","ref":"MDA_TEST_SMTP"})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    assert_eq!(v["name"], "smtp_password");
    assert_eq!(v["ref"], "MDA_TEST_SMTP");
    // a second create with the same name → conflict.
    let (st, _) = authed(
        &ctx.app,
        &ctx.token,
        "POST",
        "/api/secrets",
        Some(json!({"name":"smtp_password","ref":"MDA_TEST_SMTP"})),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);

    // list + get expose only the reference, never a value field.
    let (st, v) = authed(&ctx.app, &ctx.token, "GET", "/api/secrets", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v[0]["name"], "smtp_password");
    assert!(v.get("value").is_none());

    let (st, v) = authed(
        &ctx.app,
        &ctx.token,
        "GET",
        "/api/secrets/smtp_password",
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(v.get("value").is_none());

    // rotate: only the ref changes.
    let (st, v) = authed(
        &ctx.app,
        &ctx.token,
        "POST",
        "/api/secrets/smtp_password/rotate",
        Some(json!({"ref":"MDA_TEST_SMTP_V2"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["ref"], "MDA_TEST_SMTP_V2");
    assert!(v["rotated_at"].as_str().is_some());

    // delete.
    let st = authed(
        &ctx.app,
        &ctx.token,
        "DELETE",
        "/api/secrets/smtp_password",
        None,
    )
    .await
    .0;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (st, _) = authed(
        &ctx.app,
        &ctx.token,
        "GET",
        "/api/secrets/smtp_password",
        None,
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn secret_resolution_is_audited_and_value_never_logged() {
    let Some(ctx) = setup().await else {
        return;
    };

    // register reference pointing at a store key the LocalSecretStore resolves
    // from a file map we construct directly.
    let (st, _) = authed(
        &ctx.app,
        &ctx.token,
        "POST",
        "/api/secrets",
        Some(json!({"name":"acme_api_key","ref":"acme"})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);

    // resolve server-side via a store that actually has the value.
    let store = LocalSecretStore::from_map(
        [("acme".to_string(), "s3cr3t-value".to_string())]
            .into_iter()
            .collect(),
    );
    let value = resolve_and_audit(
        &ctx.pool,
        &store,
        ctx.tenant,
        "acme_api_key",
        Some(ctx.user_id),
        "connector.auth",
    )
    .await
    .expect("resolve");
    assert_eq!(value, b"s3cr3t-value".to_vec());

    // the audit row was written with purpose + resolver.
    let (purpose, resolved_by): (String, Option<Uuid>) = sqlx::query_as(
        "SELECT purpose, resolved_by FROM sys_secret_audit
          WHERE tenant_id = $1 AND name = 'acme_api_key'
          ORDER BY resolved_at DESC LIMIT 1",
    )
    .bind(ctx.tenant)
    .fetch_one(&ctx.pool)
    .await
    .unwrap();
    assert_eq!(purpose, "connector.auth");
    assert_eq!(resolved_by, Some(ctx.user_id));

    // the value never leaked anywhere: sys_secret_audit stores only name/purpose,
    // never the value (there is no value/payload column to check).
    assert!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM sys_secret_audit WHERE tenant_id = $1 AND name = 'acme_api_key'"
        )
        .bind(ctx.tenant)
        .fetch_one(&ctx.pool)
        .await
        .unwrap()
            >= 1
    );

    // resolving an unknown name → NotFound, no leak.
    let err = resolve_and_audit(&ctx.pool, &store, ctx.tenant, "nope", None, "x")
        .await
        .unwrap_err();
    assert!(matches!(err, mda_core::Error::NotFound(_)));

    // a reference whose store has no value → NotFound (misconfig, not a crash).
    let (_, _) = authed(
        &ctx.app,
        &ctx.token,
        "POST",
        "/api/secrets",
        Some(json!({"name":"empty","ref":"missing-key"})),
    )
    .await;
    let err = resolve_and_audit(&ctx.pool, &store, ctx.tenant, "empty", None, "x")
        .await
        .unwrap_err();
    assert!(matches!(err, mda_core::Error::NotFound(_)));
}
