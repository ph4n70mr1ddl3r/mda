//! Authentication (PLAN §3): JWT login / refresh / me, and the [`AuthUser`]
//! extractor that resolves a bearer token to an [`Identity`]. Tenant isolation
//! comes from the verified token — the client no longer supplies the tenant.

use async_trait::async_trait;
use axum::extract::{ConnectInfo, FromRequestParts, State};
use axum::http::{request::Parts, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use mda_core::Error;
use mda_security::{hash_password, load_identity, verify_password, Identity, LoginThrottle};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::ApiResult;
use crate::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/auth/login", post(login))
        .route("/api/auth/refresh", post(refresh))
        .route("/api/auth/logout", post(logout))
        .route("/api/auth/event-ticket", post(event_ticket))
}

/// The authenticated principal, extracted from `Authorization: Bearer <access>`.
pub struct AuthUser(pub Identity);

#[async_trait]
impl FromRequestParts<AppState> for AuthUser {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = bearer_token(parts).ok_or_else(|| unauthorized("missing bearer token"))?;
        let claims = state
            .jwt
            .verify_access(&token)
            .map_err(|_| unauthorized("invalid or expired token"))?;
        let identity = identity_from_claims(state, &claims).await?;
        Ok(AuthUser(identity))
    }
}

/// Resolve already-verified claims to an [`Identity`] (parse subject/tenant →
/// load). Shared by [`AuthUser`] (access tokens) and the SSE handler (tickets),
/// so the two entry points can't drift.
pub async fn identity_from_claims(
    state: &AppState,
    claims: &mda_security::jwt::Claims,
) -> Result<Identity, Response> {
    let user_id =
        Uuid::parse_str(&claims.sub).map_err(|_| unauthorized("malformed token subject"))?;
    let tenant_id =
        Uuid::parse_str(&claims.tenant).map_err(|_| unauthorized("malformed token tenant"))?;
    load_identity(&state.pool, user_id, tenant_id)
        .await
        .map_err(|_| unauthorized("user not found or inactive"))
}

fn bearer_token(parts: &Parts) -> Option<String> {
    parts
        .headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(bearer_value)
}

/// Extract a bearer token from an `Authorization` header value (shared by the
/// `FromRequestParts` extractor and handlers that take `HeaderMap` directly).
fn bearer_value(h: &axum::http::HeaderValue) -> Option<String> {
    let s = h.to_str().ok()?;
    let t = s.strip_prefix("Bearer ")?;
    Some(t.trim().to_string())
}

/// Bearer token from a `HeaderMap` (handlers like `logout` that don't use `AuthUser`).
fn bearer(headers: &HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(bearer_value)
}

fn unauthorized(msg: &str) -> Response {
    // Same four-key shape as the ADR-0018 envelope (code/error/status/message)
    // so SDK/i18n clients can key on `code` for auth failures too.
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({
            "code": "mda.unauthorized",
            "error": "unauthorized",
            "status": 401,
            "message": msg,
        })),
    )
        .into_response()
}

#[derive(Deserialize)]
struct LoginReq {
    /// Tenant identifier: a slug (e.g. "acme") or a tenant UUID. Resolved to a
    /// tenant_id so the sec_user lookup can run under that tenant's GUC (RLS).
    tenant: String,
    email: String,
    password: String,
}

#[derive(Serialize)]
struct TokenResp {
    access_token: String,
    refresh_token: String,
    token_type: &'static str,
}

async fn login(
    State(st): State<AppState>,
    headers: HeaderMap,
    conn: Option<ConnectInfo<std::net::SocketAddr>>,
    Json(req): Json<LoginReq>,
) -> ApiResult<Json<TokenResp>> {
    let throttle = st.login_throttle;
    let ip_key = client_ip(&headers, conn.as_ref()).map(|ip| LoginThrottle::ip_key(&ip));

    // 1) Per-IP lockout — checked first; it needs no tenant.
    if let Some(ipk) = &ip_key {
        if throttle.is_locked(&st.pool, ipk).await? {
            return Err(Error::RateLimited(
                "too many login attempts from this address; try again later".into(),
            )
            .into());
        }
    }

    // 2) Resolve the tenant (slug or UUID). On an unknown tenant there's no
    //    account key to form, but we still consume an IP attempt and return the
    //    same message as a bad password (no user enumeration).
    let tenant_id = match resolve_tenant(&st.pool, req.tenant.trim()).await? {
        Some(id) => id,
        None => {
            if let Some(ipk) = &ip_key {
                throttle.record_failure(&st.pool, ipk).await?;
            }
            return Err(Error::Invalid("invalid credentials".into()).into());
        }
    };

    // 3) Per-account lockout.
    let acct_key = LoginThrottle::account_key(tenant_id, &req.email);
    if throttle.is_locked(&st.pool, &acct_key).await? {
        return Err(
            Error::RateLimited("too many failed login attempts; try again later".into()).into(),
        );
    }

    // 4) Verify under the tenant GUC (sec_user is RLS-gated). Same
    //    "invalid credentials" for unknown-user and bad-password (no enumeration).
    let verified = verify_login(&st.pool, tenant_id, &req.email, &req.password).await?;
    match verified {
        Some(user_id) => {
            throttle.record_success(&st.pool, &acct_key).await?;
            if let Some(ipk) = &ip_key {
                throttle.record_success(&st.pool, ipk).await?;
            }
            // Create a revocable session; both tokens carry its id so refresh
            // rotation and logout can act on it.
            let sid = mda_security::session::create(
                &st.pool,
                tenant_id,
                user_id,
                st.jwt.refresh_ttl(),
                client_ip(&headers, conn.as_ref()).as_deref(),
            )
            .await?;
            let tokens = st.jwt.issue_pair(user_id, tenant_id, sid)?;
            Ok(Json(TokenResp {
                access_token: tokens.access,
                refresh_token: tokens.refresh,
                token_type: "Bearer",
            }))
        }
        None => {
            throttle.record_failure(&st.pool, &acct_key).await?;
            if let Some(ipk) = &ip_key {
                throttle.record_failure(&st.pool, ipk).await?;
            }
            Err(Error::Invalid("invalid credentials".into()).into())
        }
    }
}

/// Resolve a tenant slug/UUID → tenant_id. A bare UUID is accepted as-is (a
/// non-existent tenant then fails at the user lookup, fail-closed).
async fn resolve_tenant(pool: &sqlx::PgPool, tenant: &str) -> ApiResult<Option<Uuid>> {
    if let Ok(id) = Uuid::parse_str(tenant) {
        return Ok(Some(id));
    }
    let row: Option<(Uuid,)> =
        sqlx::query_as("SELECT id FROM sec.sec_tenant WHERE slug = $1 AND active = TRUE")
            .bind(tenant)
            .fetch_optional(pool)
            .await
            .map_err(Error::internal)?;
    Ok(row.map(|(id,)| id))
}

/// Tenant-scoped credential check: set the GUC, look up `sec_user` (RLS shows
/// only this tenant's rows → an email from another tenant is invisible), verify
/// the password. Returns `Some(user_id)` on success, `None` on any failure
/// (unknown user or bad password). Read-only, so the tx is committed (closed).
async fn verify_login(
    pool: &sqlx::PgPool,
    tenant: Uuid,
    email: &str,
    password: &str,
) -> ApiResult<Option<Uuid>> {
    let mut tx = pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, tenant).await?;
    let row: Option<(Uuid, Uuid, String)> = sqlx::query_as(
        "SELECT id, tenant_id, password_hash FROM sec.sec_user \
          WHERE email = $1 AND active = TRUE",
    )
    .bind(email)
    .fetch_optional(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    let Some((user_id, _, hash)) = row else {
        // Unknown user: still burn the Argon2 work against a fixed dummy hash —
        // returning early would leak account existence through response time.
        let _ = verify_password(password, dummy_hash());
        return Ok(None);
    };
    if !verify_password(password, &hash) {
        return Ok(None);
    }
    Ok(Some(user_id))
}

/// A fixed Argon2 hash with the platform's default parameters, computed once —
/// verifying against it equalizes the unknown-user login path's timing with the
/// bad-password path (no account-existence side channel).
fn dummy_hash() -> &'static str {
    static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    DUMMY.get_or_init(|| hash_password("mda-timing-equalizer").expect("hash dummy password"))
}

/// Best-effort client IP for throttling. Forwarding headers (`X-Forwarded-For`,
/// `X-Real-IP`) are honored only when the operator opted in with
/// `MDA_TRUST_PROXY=1` — otherwise they are client-spoofable and would defeat
/// the per-IP lockout. Without the opt-in (or with no headers), the TCP peer is
/// used.
fn client_ip(
    headers: &HeaderMap,
    conn: Option<&ConnectInfo<std::net::SocketAddr>>,
) -> Option<String> {
    static TRUST_PROXY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let trust_proxy = *TRUST_PROXY.get_or_init(|| {
        std::env::var("MDA_TRUST_PROXY")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    });
    if trust_proxy {
        let from_header = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| {
                    let ip = s.split(',').next().unwrap_or("").trim();
                    (!ip.is_empty()).then(|| ip.to_string())
                })
        };
        if let Some(ip) = from_header("x-forwarded-for").or_else(|| from_header("x-real-ip")) {
            return Some(ip);
        }
    }
    conn.map(|c| c.0.ip().to_string())
}

#[derive(Deserialize)]
struct RefreshReq {
    refresh_token: String,
}

async fn refresh(
    State(st): State<AppState>,
    Json(req): Json<RefreshReq>,
) -> ApiResult<Json<serde_json::Value>> {
    // A refresh token is verified (signature + type) then matched to a live
    // session, which is rotated. Reuse of an already-rotated session revokes
    // every session for the user (refresh-token-theft containment) and rejects.
    let claims = st
        .jwt
        .verify_refresh(&req.refresh_token)
        .map_err(|_| Error::Invalid("invalid refresh token".into()))?;
    let user_id = Uuid::parse_str(&claims.sub).map_err(Error::internal)?;
    let tenant_id = Uuid::parse_str(&claims.tenant).map_err(Error::internal)?;
    let sid = claims
        .sid
        .as_deref()
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(|| Error::Invalid("invalid refresh token".into()))?;
    match mda_security::session::rotate(&st.pool, tenant_id, user_id, sid, st.jwt.refresh_ttl())
        .await?
    {
        mda_security::session::RotateOutcome::Rotated(new_sid) => {
            let tokens = st.jwt.issue_pair(user_id, tenant_id, new_sid)?;
            Ok(Json(serde_json::json!({
                "access_token": tokens.access,
                "refresh_token": tokens.refresh,
                "token_type": "Bearer",
            })))
        }
        mda_security::session::RotateOutcome::Stale => {
            Err(Error::Invalid("invalid refresh token".into()).into())
        }
    }
}

/// `POST /api/auth/logout` — revoke the caller's session (the access token's
/// `sid`), so its refresh token can no longer rotate. Access tokens themselves
/// are stateless (≤15 m), so an outstanding one still works until it expires.
async fn logout(State(st): State<AppState>, headers: HeaderMap) -> ApiResult<StatusCode> {
    if let Some(token) = bearer(&headers) {
        if let Ok(claims) = st.jwt.verify_access(&token) {
            if let Some(sid) = claims.sid.as_deref().and_then(|s| Uuid::parse_str(s).ok()) {
                let tenant = Uuid::parse_str(&claims.tenant).map_err(Error::internal)?;
                mda_security::session::revoke(&st.pool, tenant, sid).await?;
            }
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/auth/event-ticket` — issue a one-shot, short-lived ticket for the
/// SSE stream (browser `EventSource` can't set headers). The ticket carries no
/// privileges of its own — the events handler resolves it to an identity — so
/// the access JWT never has to appear in a URL.
async fn event_ticket(
    AuthUser(id): AuthUser,
    State(st): State<AppState>,
) -> ApiResult<Json<serde_json::Value>> {
    let ticket = st.jwt.issue_ticket(id.user_id, id.tenant_id)?;
    Ok(Json(serde_json::json!({
        "ticket": ticket,
        "token_type": "ticket",
        "expires_in": st.jwt.ticket_ttl_secs(),
    })))
}

/// `GET /api/auth/me` (registered in the main router where AuthUser is available).
pub async fn me(AuthUser(id): AuthUser) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "user_id": id.user_id,
        "tenant_id": id.tenant_id,
        "team_id": id.team_id,
        "is_superuser": id.is_superuser,
    }))
}

/// `hash_password` re-export (for the bootstrap admin seeding in mda-server).
pub fn _hash(plain: &str) -> mda_core::Result<String> {
    hash_password(plain)
}
