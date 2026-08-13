//! Record & field history as a surfaced platform capability (PLAN §14:
//! "Record / field history as a surfaced capability").
//!
//! `sys_audit_log` already stores a before/after JSONB snapshot for every write
//! (compliance, §4.7). This module exposes that material as a tenant-facing API:
//!
//!   - `GET /api/data/:entity/:id/history` — a timeline of changes for a record,
//!     with per-field diffs. Each side is projected through field-level security
//!     so a viewer learns only about fields they may read.
//!   - `GET /api/data/:entity/:id/as-of?version=N` (or `?at=<RFC3339>`) —
//!     reconstruct the record's state at a point/version directly from the audit
//!     snapshots. Useful for "what did this look like when the invoice was
//!     approved" and for as-of reporting.
//!
//! ## Authorisation
//! History is record-scoped, exactly like a live read:
//!   - object-level `read` on the entity (else 403);
//!   - record-level: the caller must be able to read the *live* record, OR be a
//!     superuser (a record's history outlives the live row; a non-owner who
//!     could never read it must not learn its past). A deleted record's history
//!     is therefore admin/forensics-only in v1 — restore is already admin-only.
//!   - field-level: `before`/`after` are projected through the caller's FLS, so
//!     a field the viewer can't read never appears in a diff or an as-of view.
//!
//! The reconstruction is read straight from `sys_audit_log` (no new write path,
//! no separate history store) — the same source the real-time channel and the
//! compliance trail use, so the three never disagree.

use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use mda_core::Error;
use mda_security::Identity;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::data::{authorize, entity_def, project, scope_for};
use crate::error::ApiResult;
use crate::AppState;

/// Internal columns that exist on every record but are not "business fields" —
/// they must never be reported as a changed field in a diff.
const INTERNAL_COLS: &[&str] = &["id", "version", "updated_at", "created_at"];

/// One `sys_audit_log` row as selected for the timeline (id, at, actor, op,
/// before, after).
type AuditRow = (
    Uuid,
    chrono::DateTime<chrono::Utc>,
    Option<Uuid>,
    String,
    Option<Value>,
    Option<Value>,
);

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/data/:entity/:id/history", get(record_history))
        .route("/api/data/:entity/:id/as-of", get(record_as_of))
}

#[derive(Deserialize, Default)]
struct HistoryQuery {
    #[serde(default)]
    limit: Option<i64>,
}

/// One entry in a record's change timeline.
#[derive(Serialize)]
struct HistoryEntry {
    /// Audit row id (stable handle for this change).
    id: Uuid,
    /// When the change was applied.
    at: chrono::DateTime<chrono::Utc>,
    /// Who applied it (null if unknown/system).
    actor_id: Option<Uuid>,
    /// create | update | delete.
    op: String,
    /// The record version *after* this change (null if unavailable).
    version: Option<i64>,
    /// Per-field diff, field-level-security projected. Empty for a delete or
    /// when the caller may read no changed field.
    changes: Vec<FieldChange>,
}

#[derive(Serialize)]
struct FieldChange {
    field: String,
    /// `null` when the field did not exist before (first set on create) or the
    /// caller may not read the before-value.
    from: Option<Value>,
    to: Option<Value>,
}

/// `GET /api/data/:entity/:id/history` — the change timeline for a record.
async fn record_history(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path((entity, id)): Path<(String, Uuid)>,
    Query(q): Query<HistoryQuery>,
) -> ApiResult<Json<Value>> {
    let def = prepare(&st, &user, &entity, id).await?;

    let limit = q.limit.unwrap_or(100).clamp(1, 500);

    let rows: Vec<AuditRow> = sqlx::query_as(
        "SELECT id, created_at, actor_id, op, before, after
               FROM sys_audit_log
              WHERE tenant_id = $1 AND entity = $2 AND record_id = $3
              ORDER BY created_at DESC, id DESC
              LIMIT $4",
    )
    .bind(user.tenant_id)
    .bind(&entity)
    .bind(id)
    .bind(limit)
    .fetch_all(&st.pool)
    .await
    .map_err(Error::internal)?;

    let entries: Vec<HistoryEntry> = rows
        .into_iter()
        .map(|(rid, at, actor_id, op, before, after)| HistoryEntry {
            id: rid,
            at,
            actor_id,
            version: after.as_ref().and_then(version_of),
            op,
            changes: diff(&user, &entity, &def, &before, &after),
        })
        .collect();

    Ok(Json(json!({
        "entity": entity,
        "record_id": id,
        "entries": entries,
    })))
}

#[derive(Deserialize)]
struct AsOfQuery {
    /// Reconstruct the state at this record version (`after.version == N`).
    version: Option<i64>,
    /// …or at this instant (RFC-3339). Mutually exclusive with `version`.
    at: Option<String>,
}

/// `GET /api/data/:entity/:id/as-of?version=N` (or `?at=<RFC3339>`) — the record
/// reconstructed from audit snapshots. Returns 404 (`mda.not_found`) if the
/// record did not exist at that point or was deleted by then.
async fn record_as_of(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path((entity, id)): Path<(String, Uuid)>,
    Query(q): Query<AsOfQuery>,
) -> ApiResult<Json<Value>> {
    let def = prepare(&st, &user, &entity, id).await?;

    let snapshot: Option<(Option<Value>, String)> = match (q.version, q.at) {
        (Some(v), None) => sqlx::query_as(
            "SELECT after, op FROM sys_audit_log
              WHERE tenant_id = $1 AND entity = $2 AND record_id = $3
                AND op IN ('create','update')
                AND (after ->> 'version')::bigint = $4
              ORDER BY created_at DESC, id DESC LIMIT 1",
        )
        .bind(user.tenant_id)
        .bind(&entity)
        .bind(id)
        .bind(v)
        .fetch_optional(&st.pool)
        .await
        .map_err(Error::internal)?,
        (None, Some(iso)) => {
            let ts: chrono::DateTime<chrono::Utc> = iso
                .parse()
                .map_err(|e| Error::Invalid(format!("bad 'at' timestamp: {e}")))?;
            sqlx::query_as(
                "SELECT after, op FROM sys_audit_log
                  WHERE tenant_id = $1 AND entity = $2 AND record_id = $3
                    AND created_at <= $4
                  ORDER BY created_at DESC, id DESC LIMIT 1",
            )
            .bind(user.tenant_id)
            .bind(&entity)
            .bind(id)
            .bind(ts)
            .fetch_optional(&st.pool)
            .await
            .map_err(Error::internal)?
        }
        (None, None) => {
            return Err(Error::Invalid("provide exactly one of ?version= or ?at=".into()).into());
        }
        (Some(_), Some(_)) => {
            return Err(Error::Invalid("?version= and ?at= are mutually exclusive".into()).into());
        }
    };

    let (after, op) = match snapshot {
        Some(s) => s,
        None => {
            return Err(Error::NotFound(
                "no snapshot at the requested point: record did not exist yet".into(),
            )
            .into());
        }
    };

    // If the latest op at/before the point was a delete, the record was gone.
    if op == "delete" {
        return Err(
            Error::NotFound("record had been deleted by the requested point".into()).into(),
        );
    }

    let reconstructed = after.unwrap_or_else(|| json!({}));
    Ok(Json(project(&user, &entity, &def, reconstructed)))
}

// ===== shared authz + lookup =====

/// Object + record + existence gating shared by both endpoints. Returns the
/// entity definition (needed for FLS projection of snapshots).
async fn prepare(
    st: &AppState,
    user: &Identity,
    entity: &str,
    id: Uuid,
) -> ApiResult<std::sync::Arc<mda_meta::EntityDefinition>> {
    authorize(user, entity, "read")?;
    let def = entity_def(st, user.tenant_id, entity).await?;

    // Record-level: must be able to read the LIVE record, unless superuser
    // (a deleted record's history is forensics/admin-only in v1). Reading is
    // also how we 404 an unknown id without leaking existence via timing.
    if !user.is_superuser {
        let scope = scope_for(st, user, entity).await?;
        mda_data::read(&st.pool, user.tenant_id, &def, id, &scope).await?;
    }
    Ok(def)
}

/// Compute a field-level diff between before/after, projecting both sides
/// through the caller's field-level read security. Internal versioning columns
/// are never reported as changes.
fn diff(
    user: &Identity,
    entity: &str,
    def: &mda_meta::EntityDefinition,
    before: &Option<Value>,
    after: &Option<Value>,
) -> Vec<FieldChange> {
    let empty = Map::new();
    let b = before
        .as_ref()
        .and_then(|v| v.as_object())
        .unwrap_or(&empty);
    let a = after.as_ref().and_then(|v| v.as_object()).unwrap_or(&empty);

    // The set of fields to consider: every key on either side, minus internals.
    let mut keys: Vec<&String> = b.keys().chain(a.keys()).collect();
    keys.sort();
    keys.dedup();

    keys.into_iter()
        .filter(|k| !INTERNAL_COLS.contains(&k.as_str()))
        .filter_map(|k| {
            // Field-level security: only report a field the caller may read.
            // Skip metadata fields not in the definition (e.g. legacy columns)
            // unless the caller is superuser (def lookup returns Write then).
            let in_def = def.fields.iter().any(|f| &f.name == k);
            if !user.is_superuser && !in_def {
                return None;
            }
            if user.field_access(entity, k) == mda_security::Access::None {
                return None;
            }
            let from = b.get(k).cloned();
            let to = a.get(k).cloned();
            if from == to {
                None // unchanged on this op
            } else {
                Some(FieldChange {
                    field: k.clone(),
                    from,
                    to,
                })
            }
        })
        .collect()
}

fn version_of(v: &Value) -> Option<i64> {
    v.get("version").and_then(|x| x.as_i64())
}
