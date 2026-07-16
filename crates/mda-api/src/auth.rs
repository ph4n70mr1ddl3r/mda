//! Authentication (PLAN §3): JWT login / refresh / me, and the [`AuthUser`]
//! extractor that resolves a bearer token to an [`Identity`]. Tenant isolation
//! comes from the verified token — the client no longer supplies the tenant.

use async_trait::async_trait;
use axum::extract::{FromRequestParts, State};
use axum::http::{request::Parts, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use mda_core::Error;
use mda_security::{hash_password, load_identity, verify_password, Identity};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::ApiResult;
use crate::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/auth/login", post(login))
        .route("/api/auth/refresh", post(refresh))
}

/// The authenticated principal, extracted from `Authorization: Bearer <token>`.
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
            .verify(&token)
            .map_err(|_| unauthorized("invalid or expired token"))?;
        let user_id =
            Uuid::parse_str(&claims.sub).map_err(|_| unauthorized("malformed token subject"))?;
        let identity = load_identity(&state.pool, user_id)
            .await
            .map_err(|_| unauthorized("user not found or inactive"))?;
        Ok(AuthUser(identity))
    }
}

fn bearer_token(parts: &Parts) -> Option<String> {
    let h = parts.headers.get(axum::http::header::AUTHORIZATION)?;
    let s = h.to_str().ok()?;
    let t = s.strip_prefix("Bearer ")?;
    Some(t.trim().to_string())
}

fn unauthorized(msg: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({ "error": "unauthorized", "message": msg })),
    )
        .into_response()
}

#[derive(Deserialize)]
struct LoginReq {
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
    Json(req): Json<LoginReq>,
) -> ApiResult<Json<TokenResp>> {
    let row: Option<(Uuid, Uuid, String)> = sqlx::query_as(
        "SELECT id, tenant_id, password_hash FROM sec.sec_user WHERE email = $1 AND active = TRUE",
    )
    .bind(&req.email)
    .fetch_optional(&st.pool)
    .await
    .map_err(Error::internal)?;
    let (user_id, tenant_id, hash) =
        row.ok_or_else(|| Error::Invalid("invalid credentials".into()))?;
    if !verify_password(&req.password, &hash) {
        return Err(Error::Invalid("invalid credentials".into()).into());
    }
    let tokens = st.jwt.issue_pair(user_id, tenant_id)?;
    Ok(Json(TokenResp {
        access_token: tokens.access,
        refresh_token: tokens.refresh,
        token_type: "Bearer",
    }))
}

#[derive(Deserialize)]
struct RefreshReq {
    refresh_token: String,
}

async fn refresh(
    State(st): State<AppState>,
    Json(req): Json<RefreshReq>,
) -> ApiResult<Json<serde_json::Value>> {
    let claims = st
        .jwt
        .verify(&req.refresh_token)
        .map_err(|_| Error::Invalid("invalid refresh token".into()))?;
    let user_id = Uuid::parse_str(&claims.sub).map_err(Error::internal)?;
    let tenant_id = Uuid::parse_str(&claims.tenant).map_err(Error::internal)?;
    let access = st.jwt.issue_access(user_id, tenant_id)?;
    Ok(Json(
        serde_json::json!({ "access_token": access, "token_type": "Bearer" }),
    ))
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
