//! Attachments & blob storage (PLAN §5.14): a `BlobStore` abstraction with a
//! local-FS implementation; `sys_blob` holds metadata only. Upload/download are
//! authenticated; an attachment field stores a blob id.
//!
//! Phase-10 MVP: owner-based access (record/field-level attachment AuthZ is a
//! refinement); S3/virus-scan/dedup/orphan-cleanup are follow-ups.

use std::path::PathBuf;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use mda_core::{Error, Result};
use serde::Serialize;
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::error::ApiResult;
use crate::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/attachments", post(upload_attachment))
        .route("/api/attachments/:id", get(download_attachment))
}

trait BlobStore {
    fn put(&self, key: &str, bytes: &[u8]) -> Result<()>;
    fn get(&self, key: &str) -> Result<Vec<u8>>;
}

struct LocalBlobStore(PathBuf);

impl LocalBlobStore {
    fn from_env() -> Self {
        let dir = std::env::var("MDA_BLOB_DIR").unwrap_or_else(|_| "/tmp/mda-blobs".to_string());
        let p = PathBuf::from(dir);
        let _ = std::fs::create_dir_all(&p);
        Self(p)
    }
}

impl BlobStore for LocalBlobStore {
    fn put(&self, key: &str, bytes: &[u8]) -> Result<()> {
        std::fs::write(self.0.join(key), bytes).map_err(Error::internal)
    }
    fn get(&self, key: &str) -> Result<Vec<u8>> {
        std::fs::read(self.0.join(key)).map_err(Error::internal)
    }
}

#[derive(Serialize)]
struct BlobInfo {
    id: Uuid,
    filename: Option<String>,
    mime: Option<String>,
    size: i64,
}

/// `POST /api/attachments` (raw body = bytes; `x-filename` + content-type headers).
async fn upload_attachment(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<BlobInfo>)> {
    let id = Uuid::from(mda_core::Id::new());
    let key = id.to_string();
    let filename = headers
        .get("x-filename")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let mime = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let size = body.len() as i64;

    LocalBlobStore::from_env().put(&key, &body)?;
    sqlx::query(
        "INSERT INTO sys_blob (id, tenant_id, storage, storage_key, filename, mime, size, owner_id)
         VALUES ($1, $2, 'local', $3, $4, $5, $6, $7)",
    )
    .bind(id)
    .bind(user.tenant_id)
    .bind(&key)
    .bind(&filename)
    .bind(&mime)
    .bind(size)
    .bind(user.user_id)
    .execute(&st.pool)
    .await
    .map_err(Error::internal)?;

    Ok((
        StatusCode::CREATED,
        Json(BlobInfo {
            id,
            filename,
            mime,
            size,
        }),
    ))
}

/// `GET /api/attachments/:id` — owner (or superuser) may download.
async fn download_attachment(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<Response> {
    let row: Option<(Uuid, Option<String>, Option<String>, String)> =
        sqlx::query_as("SELECT tenant_id, filename, mime, storage_key FROM sys_blob WHERE id = $1")
            .bind(id)
            .fetch_optional(&st.pool)
            .await
            .map_err(Error::internal)?;
    let (tenant, filename, mime, key) = row.ok_or_else(|| Error::NotFound(format!("blob {id}")))?;
    if tenant != user.tenant_id {
        return Err(Error::NotFound(format!("blob {id}")).into());
    }
    // owner-based access (record/field attachment AuthZ is a refinement)
    let owner: Option<(Uuid,)> =
        sqlx::query_as("SELECT owner_id FROM sys_blob WHERE id = $1 AND tenant_id = $2")
            .bind(id)
            .bind(user.tenant_id)
            .fetch_optional(&st.pool)
            .await
            .map_err(Error::internal)?;
    let allowed = user.is_superuser || owner.map(|(o,)| o == user.user_id).unwrap_or(false);
    if !allowed {
        return Err(Error::Forbidden("not the blob owner".into()).into());
    }

    let bytes = LocalBlobStore::from_env().get(&key)?;
    let ct = mime.unwrap_or_else(|| "application/octet-stream".to_string());
    let mut resp = (StatusCode::OK, bytes).into_response();
    resp.headers_mut()
        .insert(header::CONTENT_TYPE, ct.parse().unwrap());
    if let Some(name) = filename {
        if let Ok(hv) = format!("attachment; filename=\"{name}\"").parse() {
            resp.headers_mut().insert(header::CONTENT_DISPOSITION, hv);
        }
    }
    Ok(resp)
}
