//! Attachments & blob storage (PLAN §5.14): a `BlobStore` abstraction with a
//! local-FS implementation; `sys_blob` holds metadata only (incl. a sha256
//! `checksum`). Upload/download are authenticated; an attachment field stores a
//! blob id. Same-bytes uploads within a tenant dedup to one stored blob (§5.14
//! “Dedup by checksum”), and a delete is refcount-aware so a shared blob is only
//! removed from the store once its last metadata row goes. S3 store,
//! virus-scan, thumbnails, and the record→blob `sys_blob_ref` lifecycle hook
//! (clear-on-field-clear / cascade cleanup) remain follow-ups.

use std::path::PathBuf;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use mda_core::{Error, Result};
use serde::Serialize;
use sha2::Digest;
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::error::ApiResult;
use crate::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/attachments", post(upload_attachment))
        .route(
            "/api/attachments/:id",
            get(download_attachment).delete(delete_attachment),
        )
}

/// Storage backend for attachments. Thread-safe (Send + Sync) so it can live
/// in [`crate::AppState`] and be shared across handler invocations.
pub trait BlobStore: Send + Sync {
    fn put(&self, key: &str, bytes: &[u8]) -> Result<()>;
    fn get(&self, key: &str) -> Result<Vec<u8>>;
    /// Remove a blob. Missing blobs are not an error (idempotent / refcount
    /// cleanup may have already reclaimed the bytes).
    fn delete(&self, key: &str) -> Result<()>;
}

/// Filesystem-backed blob storage.
#[derive(Clone)]
pub struct LocalBlobStore(std::sync::Arc<PathBuf>);

impl LocalBlobStore {
    /// Create a local-FS store rooted at the directory given by
    /// `MDA_BLOB_DIR` (default `/tmp/mda-blobs`). The directory is created
    /// if it doesn't exist.
    pub fn from_env() -> Self {
        let dir = std::env::var("MDA_BLOB_DIR").unwrap_or_else(|_| "/tmp/mda-blobs".to_string());
        let p = PathBuf::from(dir);
        let _ = std::fs::create_dir_all(&p);
        Self(std::sync::Arc::new(p))
    }
}

impl BlobStore for LocalBlobStore {
    fn put(&self, key: &str, bytes: &[u8]) -> Result<()> {
        std::fs::write(self.0.join(key), bytes).map_err(Error::internal)
    }
    fn get(&self, key: &str) -> Result<Vec<u8>> {
        std::fs::read(self.0.join(key)).map_err(Error::internal)
    }
    fn delete(&self, key: &str) -> Result<()> {
        match std::fs::remove_file(self.0.join(key)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::internal(e)),
        }
    }
}

#[derive(Serialize)]
struct BlobInfo {
    id: Uuid,
    filename: Option<String>,
    mime: Option<String>,
    size: i64,
    checksum: String,
}

/// `POST /api/attachments` (raw body = bytes; `x-filename` + content-type headers).
/// Computes a sha256 checksum and dedups by `(tenant, checksum)`: two uploads of
/// the same bytes share one stored blob (a fresh metadata row per upload, so
/// ownership/metadata stay independent). §5.14 “Dedup by checksum” + integrity.
async fn upload_attachment(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<BlobInfo>)> {
    let id = Uuid::from(mda_core::Id::new());
    let size = body.len() as i64;
    let checksum = hex::encode(sha2::Sha256::digest(&body));
    let filename = headers
        .get("x-filename")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let mime = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    // Dedup lookup + insert in one txn so two concurrent same-bytes uploads
    // can’t both miss the cache (at worst both write the file; dedup still
    // holds afterwards). sys_blob is app-layer tenant-filtered (no RLS).
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    let existing: Option<(String,)> =
        sqlx::query_as("SELECT storage_key FROM sys_blob WHERE tenant_id = $1 AND checksum = $2 AND storage = 'local' LIMIT 1")
            .bind(user.tenant_id)
            .bind(&checksum)
            .fetch_optional(&mut *tx)
            .await
            .map_err(Error::internal)?;
    let key = if let Some((k,)) = existing {
        k // reuse the stored bytes
    } else {
        let k = id.to_string();
        st.blobs.put(&k, &body)?;
        k
    };
    sqlx::query(
        "INSERT INTO sys_blob (id, tenant_id, storage, storage_key, filename, mime, size, checksum, owner_id)
         VALUES ($1, $2, 'local', $3, $4, $5, $6, $7, $8)",
    )
    .bind(id)
    .bind(user.tenant_id)
    .bind(&key)
    .bind(&filename)
    .bind(&mime)
    .bind(size)
    .bind(&checksum)
    .bind(user.user_id)
    .execute(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;

    Ok((
        StatusCode::CREATED,
        Json(BlobInfo {
            id,
            filename,
            mime,
            size,
            checksum,
        }),
    ))
}

/// `GET /api/attachments/:id` — owner (or superuser) may download.
async fn download_attachment(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<Response> {
    // Single tenant-scoped read: metadata + owner in one round trip. A
    // cross-tenant request yields 404 (no information leak).
    #[derive(sqlx::FromRow)]
    struct BlobMeta {
        filename: Option<String>,
        mime: Option<String>,
        storage_key: String,
        owner_id: Option<Uuid>,
    }
    let row: Option<BlobMeta> =
        sqlx::query_as("SELECT filename, mime, storage_key, owner_id FROM sys_blob WHERE id = $1 AND tenant_id = $2")
            .bind(id)
            .bind(user.tenant_id)
            .fetch_optional(&st.pool)
            .await
            .map_err(Error::internal)?;
    let meta = row.ok_or_else(|| Error::NotFound(format!("blob {id}")))?;
    // owner-based access (record/field attachment AuthZ is a refinement)
    let allowed = user.is_superuser || meta.owner_id == Some(user.user_id);
    if !allowed {
        return Err(Error::Forbidden("not the blob owner".into()).into());
    }

    let bytes = st.blobs.get(&meta.storage_key)?;
    let ct = meta
        .mime
        .unwrap_or_else(|| "application/octet-stream".to_string());
    let mut resp = (StatusCode::OK, bytes).into_response();
    // Stored mime is upload-sourced, so it is header-shaped in practice — but
    // degrade to octet-stream rather than panic if it ever isn't.
    let ct: header::HeaderValue = ct
        .parse()
        .unwrap_or(header::HeaderValue::from_static("application/octet-stream"));
    resp.headers_mut().insert(header::CONTENT_TYPE, ct);
    if let Some(name) = meta.filename.as_deref().and_then(sanitize_filename) {
        if let Ok(hv) = format!("attachment; filename=\"{name}\"").parse() {
            resp.headers_mut().insert(header::CONTENT_DISPOSITION, hv);
        }
    }
    Ok(resp)
}

/// `DELETE /api/attachments/:id` — remove a blob’s metadata. Owner/superuser
/// only. Bytes are reclaimed from the store only when this was the **last**
/// metadata row pointing at them (dedup ⇒ shared storage_key), so deleting one
/// reference never orphans another (§5.14 cleanup).
async fn delete_attachment(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    // Load + authorize + delete + refcount in one txn (consistent view of who
    // else shares the storage_key). sys_blob is app-layer tenant-filtered.
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    let row: Option<(String, Option<Uuid>)> = sqlx::query_as(
        "SELECT storage_key, owner_id FROM sys_blob WHERE id = $1 AND tenant_id = $2",
    )
    .bind(id)
    .bind(user.tenant_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(Error::internal)?;
    let (storage_key, owner_id) = row.ok_or_else(|| Error::NotFound(format!("blob {id}")))?;
    if !(user.is_superuser || owner_id == Some(user.user_id)) {
        return Err(Error::Forbidden("not the blob owner".into()).into());
    }
    sqlx::query("DELETE FROM sys_blob WHERE id = $1 AND tenant_id = $2")
        .bind(id)
        .bind(user.tenant_id)
        .execute(&mut *tx)
        .await
        .map_err(Error::internal)?;
    // Any other metadata row (this tenant) still referencing the same bytes?
    let shared: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM sys_blob WHERE tenant_id = $1 AND storage_key = $2",
    )
    .bind(user.tenant_id)
    .bind(&storage_key)
    .fetch_one(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;

    if shared == 0 {
        // Best-effort: a leftover file is a minor leak; a missing file on a
        // later get is handled (NotFound). Never fail the request over cleanup.
        if let Err(e) = st.blobs.delete(&storage_key) {
            tracing::warn!(?e, %id, "blob byte-cleanup failed (refcount 0)");
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Strip bytes that would break (or inject into) a `Content-Disposition`
/// header value, then trim. `None` if nothing usable remains. The filename is
/// client-supplied at upload time, so treat it as untrusted.
fn sanitize_filename(name: &str) -> Option<String> {
    let cleaned: String = name
        .chars()
        .filter(|c| *c >= ' ' && *c != '"' && *c != '\\')
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned.to_string())
    }
}
