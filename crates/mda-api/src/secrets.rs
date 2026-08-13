//! Secrets management (PLAN §5.20).
//!
//! `sys_secret` holds only a reference; the value is resolved at runtime from a
//! [`SecretStore`]. This module ships [`LocalSecretStore`] (dev) and the REST
//! surface for registering/listing/rotating secret references.
//!
//! **Values are never returned by any API.** The list/rotate endpoints surface
//! only `(name, kind, ref, rotated_at)` — the `ref` is the store key, not the
//! secret. Resolution happens server-side only (the outbox webhook signer and
//! the integration connector auth call `resolve_and_audit`).
//!
//! `LocalSecretStore` resolution order (first hit wins):
//! 1. an environment variable named exactly `ref` (dev convenience —
//!    `MDA_SMTP_PASSWORD`, etc.);
//! 2. a JSON file at `MDA_SECRET_FILE` mapping `{ "<ref>": "<value>" }`, loaded
//!    once at construction.
//!
//! Cloud KMS / Vault impls are follow-ups (same trait).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use mda_core::{Error, Result, SecretStore};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::error::ApiResult;
use crate::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/secrets", get(list_secrets).post(create_secret))
        .route("/api/secrets/:name", get(get_secret).delete(delete_secret))
        .route("/api/secrets/:name/rotate", post(rotate_secret))
}

/// File/env-backed dev secret store. Production uses a cloud KMS / Vault impl
/// (same trait). Holds the file map behind an `Arc` so it is cheap to clone.
#[derive(Clone)]
pub struct LocalSecretStore {
    file: Arc<HashMap<String, String>>,
}

impl LocalSecretStore {
    /// Build from the environment: reads `MDA_SECRET_FILE` (a JSON object
    /// mapping `ref → value`). Missing/unreadable file → empty map (env vars
    /// still resolve). Never panics — a dev misconfig degrades to "env only".
    pub fn from_env() -> Self {
        let file = std::env::var("MDA_SECRET_FILE")
            .ok()
            .and_then(|p| std::fs::read(PathBuf::from(&p)).ok())
            .and_then(|bytes| serde_json::from_slice::<HashMap<String, String>>(&bytes).ok())
            .unwrap_or_default();
        Self {
            file: Arc::new(file),
        }
    }

    /// Build directly from a map (tests / programmatic construction).
    pub fn from_map(file: HashMap<String, String>) -> Self {
        Self {
            file: Arc::new(file),
        }
    }
}

impl SecretStore for LocalSecretStore {
    fn resolve(&self, store_ref: &str) -> Result<Option<Vec<u8>>> {
        // 1) environment variable named exactly `ref`.
        if let Ok(v) = std::env::var(store_ref) {
            return Ok(Some(v.into_bytes()));
        }
        // 2) JSON file map.
        if let Some(v) = self.file.get(store_ref) {
            return Ok(Some(v.clone().into_bytes()));
        }
        Ok(None)
    }
}

#[derive(Debug, Serialize)]
struct SecretRef {
    name: String,
    kind: String,
    r#ref: String,
    rotated_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CreateSecret {
    name: String,
    #[serde(default = "default_kind")]
    kind: String,
    r#ref: String,
}

fn default_kind() -> String {
    "opaque".to_string()
}

/// Resolve a secret value by its modeler-facing name under the tenant, recording
/// an audit row. Used by the outbox webhook signer and the integration connector
/// auth — the only places values are touched. Returns `Error::NotFound` when the
/// reference exists but the store has no value for it (a misconfig), and
/// `Error::NotFound` for an unknown name too (no information leak).
pub async fn resolve_and_audit(
    pool: &sqlx::PgPool,
    store: &dyn SecretStore,
    tenant: Uuid,
    name: &str,
    resolved_by: Option<Uuid>,
    purpose: &str,
) -> Result<Vec<u8>> {
    let row: Option<(Uuid, String)> =
        sqlx::query_as("SELECT id, ref FROM sys_secret WHERE tenant_id = $1 AND name = $2")
            .bind(tenant)
            .bind(name)
            .fetch_optional(pool)
            .await
            .map_err(Error::internal)?;
    let (id, store_ref) = row.ok_or_else(|| Error::NotFound(format!("secret {name}")))?;

    let value = store
        .resolve(&store_ref)?
        .ok_or_else(|| Error::NotFound(format!("secret {name} has no value in the store")))?;

    sqlx::query(
        "INSERT INTO sys_secret_audit (tenant_id, secret_id, name, resolved_by, purpose)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(tenant)
    .bind(id)
    .bind(name)
    .bind(resolved_by)
    .bind(purpose)
    .execute(pool)
    .await
    .map_err(Error::internal)?;

    Ok(value)
}

async fn list_secrets(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
) -> ApiResult<Json<Vec<SecretRef>>> {
    let rows: Vec<(
        String,
        String,
        String,
        Option<chrono::DateTime<chrono::Utc>>,
    )> = sqlx::query_as(
        "SELECT name, kind, ref, rotated_at FROM sys_secret
              WHERE tenant_id = $1 ORDER BY name",
    )
    .bind(user.tenant_id)
    .fetch_all(&st.pool)
    .await
    .map_err(Error::internal)?;
    Ok(Json(
        rows.into_iter()
            .map(|(name, kind, r#ref, rotated_at)| SecretRef {
                name,
                kind,
                r#ref,
                rotated_at: rotated_at.map(|d| d.to_rfc3339()),
            })
            .collect(),
    ))
}

async fn get_secret(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(name): Path<String>,
) -> ApiResult<Json<SecretRef>> {
    let row: Option<(
        String,
        String,
        String,
        Option<chrono::DateTime<chrono::Utc>>,
    )> = sqlx::query_as(
        "SELECT name, kind, ref, rotated_at FROM sys_secret
              WHERE tenant_id = $1 AND name = $2",
    )
    .bind(user.tenant_id)
    .bind(&name)
    .fetch_optional(&st.pool)
    .await
    .map_err(Error::internal)?;
    let (name, kind, r#ref, rotated_at) =
        row.ok_or_else(|| Error::NotFound(format!("secret {name}")))?;
    Ok(Json(SecretRef {
        name,
        kind,
        r#ref,
        rotated_at: rotated_at.map(|d| d.to_rfc3339()),
    }))
}

async fn create_secret(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Json(body): Json<CreateSecret>,
) -> ApiResult<(StatusCode, Json<SecretRef>)> {
    if body.name.trim().is_empty() || body.r#ref.trim().is_empty() {
        return Err(Error::Invalid("name and ref are required".into()).into());
    }
    let inserted: Option<(
        String,
        String,
        String,
        Option<chrono::DateTime<chrono::Utc>>,
    )> = sqlx::query_as(
        "INSERT INTO sys_secret (tenant_id, name, kind, ref) VALUES ($1, $2, $3, $4)
             ON CONFLICT (tenant_id, name) DO NOTHING
             RETURNING name, kind, ref, rotated_at",
    )
    .bind(user.tenant_id)
    .bind(&body.name)
    .bind(&body.kind)
    .bind(&body.r#ref)
    .fetch_optional(&st.pool)
    .await
    .map_err(Error::internal)?;
    let row = inserted.ok_or_else(|| Error::Conflict(format!("secret {} exists", body.name)))?;
    Ok((
        StatusCode::CREATED,
        Json(SecretRef {
            name: row.0,
            kind: row.1,
            r#ref: row.2,
            rotated_at: row.3.map(|d| d.to_rfc3339()),
        }),
    ))
}

async fn delete_secret(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    let n = sqlx::query("DELETE FROM sys_secret WHERE tenant_id = $1 AND name = $2")
        .bind(user.tenant_id)
        .bind(&name)
        .execute(&st.pool)
        .await
        .map_err(Error::internal)?
        .rows_affected();
    if n == 0 {
        return Err(Error::NotFound(format!("secret {name}")).into());
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
struct RotateBody {
    /// New store ref (e.g. the new env var / KMS id). Required — rotation is
    /// explicit and never touches the value from this API.
    r#ref: String,
}

async fn rotate_secret(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(name): Path<String>,
    Json(body): Json<RotateBody>,
) -> ApiResult<Json<SecretRef>> {
    let row: Option<(
        String,
        String,
        String,
        Option<chrono::DateTime<chrono::Utc>>,
    )> = sqlx::query_as(
        "UPDATE sys_secret SET ref = $3, rotated_at = now()
              WHERE tenant_id = $1 AND name = $2
             RETURNING name, kind, ref, rotated_at",
    )
    .bind(user.tenant_id)
    .bind(&name)
    .bind(&body.r#ref)
    .fetch_optional(&st.pool)
    .await
    .map_err(Error::internal)?;
    let row = row.ok_or_else(|| Error::NotFound(format!("secret {name}")))?;
    Ok(Json(SecretRef {
        name: row.0,
        kind: row.1,
        r#ref: row.2,
        rotated_at: row.3.map(|d| d.to_rfc3339()),
    }))
}
