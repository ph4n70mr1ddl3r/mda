//! Tenant observability console (PLAN §14: "Modeler / tenant observability
//! console" + "Scheduled-job management … failure state").
//!
//! A read-only, tenant-scoped surface over the operational tables the platform
//! already writes — so a modeler/operator can see job, rule, workflow, publish,
//! and delivery run history and failures *without* raw database or
//! `tracing`/OpenTelemetry access.
//!
//!   - `GET /api/observability/events`      — recent domain events (`sys_event_log`)
//!   - `GET /api/observability/outbox`       — delivery queue health (`sys_outbox`)
//!   - `GET /api/observability/migrations`   — publish execution log (`md_migration_log`)
//!   - `GET /api/observability/audit`        — audit trail browse (`sys_audit_log`)
//!
//! ## Authorisation
//! A superuser always passes. A non-admin principal passes when granted the
//! `observability.read` capability (a `("*", "observability.read")` permission) —
//! this lets a tenant modeler/operator see run/delivery/audit history without
//! raw DB access. Audit `before`/`after` payloads are redacted for non-superuser
//! readers (they may carry sensitive field values); superusers see them verbatim.
//! All other surfaces (events/outbox/migrations) are returned in full to any
//! reader who passes the gate (they carry no field-level data).
//!
//! All queries are filtered by the caller's `tenant_id`; `sys_*` tables carry
//! no RLS (they are app-layer-isolated), and `md_migration_log` is read under
//! the tenant GUC exactly like the Studio handlers.

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use mda_core::Error;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::error::ApiResult;
use crate::AppState;

/// `sys_event_log` row for the console: seq, ts, type, entity, record_id,
/// actor_id, payload.
type EventRow = (
    i64,
    chrono::DateTime<chrono::Utc>,
    String,
    Option<String>,
    Option<Uuid>,
    Option<Uuid>,
    Value,
);

/// `sys_outbox` status breakdown row: status, count, oldest created_at.
type OutboxCountRow = (String, i64, Option<chrono::DateTime<chrono::Utc>>);

/// `sys_outbox` outstanding-work row.
type OutboxItemRow = (
    Uuid,
    String,
    String,
    i32,
    chrono::DateTime<chrono::Utc>,
    Option<chrono::DateTime<chrono::Utc>>,
);

/// `md_migration_log` joined row.
type MigrationRow = (
    Uuid,
    Uuid,
    Option<String>,
    String,
    String,
    Option<Uuid>,
    i64,
    chrono::DateTime<chrono::Utc>,
    Option<chrono::DateTime<chrono::Utc>>,
);

/// `sys_audit_log` row for the console.
type AuditRow = (
    Uuid,
    chrono::DateTime<chrono::Utc>,
    Option<Uuid>,
    String,
    Uuid,
    String,
    Option<Value>,
    Option<Value>,
);

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/observability/events", get(events))
        .route("/api/observability/outbox", get(outbox))
        .route("/api/observability/migrations", get(migrations))
        .route("/api/observability/audit", get(audit))
}

#[derive(Deserialize, Default)]
struct EventQuery {
    #[serde(default)]
    r#type: Option<String>,
    #[serde(default)]
    entity: Option<String>,
    /// seq strictly greater than this (cursor for "newer than").
    #[serde(default)]
    since: Option<i64>,
    #[serde(default)]
    limit: Option<i64>,
}

/// `GET /api/observability/events` — the canonical domain-event stream for the
/// tenant (the same `sys_event_log` the SSE relay fans out).
async fn events(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Query(q): Query<EventQuery>,
) -> ApiResult<Json<Value>> {
    require_observability(&user)?;
    let limit = q.limit.unwrap_or(100).clamp(1, 500);

    let rows: Vec<EventRow> = sqlx::query_as(
        "SELECT seq, ts, type, entity, record_id, actor_id, payload
               FROM sys_event_log
              WHERE tenant_id = $1
                AND ($2::text IS NULL OR type = $2)
                AND ($3::text IS NULL OR entity = $3)
                AND ($4::bigint IS NULL OR seq > $4)
              ORDER BY seq DESC
              LIMIT $5",
    )
    .bind(user.tenant_id)
    .bind(&q.r#type)
    .bind(&q.entity)
    .bind(q.since)
    .bind(limit)
    .fetch_all(&st.pool)
    .await
    .map_err(Error::internal)?;

    let items: Vec<Value> = rows
        .into_iter()
        .map(|(seq, ts, typ, entity, record_id, actor_id, payload)| {
            json!({
                "seq": seq,
                "ts": ts,
                "type": typ,
                "entity": entity,
                "record_id": record_id,
                "actor_id": actor_id,
                "payload": payload,
            })
        })
        .collect();
    Ok(Json(json!({ "items": items })))
}

/// `GET /api/observability/outbox` — delivery queue health: a status breakdown
/// plus the still-pending/failed entries (oldest first) so a stalled delivery is
/// visible at a glance.
async fn outbox(State(st): State<AppState>, AuthUser(user): AuthUser) -> ApiResult<Json<Value>> {
    require_observability(&user)?;

    // Status breakdown (pending / done / failed) with age of the oldest in each.
    let breakdown: Vec<OutboxCountRow> = sqlx::query_as(
        "SELECT status, count(*)::bigint, min(created_at)
           FROM sys_outbox
          WHERE tenant_id = $1
          GROUP BY status",
    )
    .bind(user.tenant_id)
    .fetch_all(&st.pool)
    .await
    .map_err(Error::internal)?;

    let counts: Vec<Value> = breakdown
        .into_iter()
        .map(|(status, n, oldest)| {
            json!({
                "status": status,
                "count": n,
                "oldest_created_at": oldest,
            })
        })
        .collect();

    // Outstanding work (not done) — the actionable list.
    let outstanding: Vec<OutboxItemRow> = sqlx::query_as(
        "SELECT id, kind, status, attempts, created_at, processed_at
               FROM sys_outbox
              WHERE tenant_id = $1 AND status <> 'done'
              ORDER BY created_at ASC
              LIMIT 200",
    )
    .bind(user.tenant_id)
    .fetch_all(&st.pool)
    .await
    .map_err(Error::internal)?;

    let items: Vec<Value> = outstanding
        .into_iter()
        .map(|(id, kind, status, attempts, created_at, processed_at)| {
            json!({
                "id": id,
                "kind": kind,
                "status": status,
                "attempts": attempts,
                "created_at": created_at,
                "processed_at": processed_at,
            })
        })
        .collect();

    Ok(Json(json!({
        "counts": counts,
        "outstanding": items,
    })))
}

/// `GET /api/observability/migrations` — publish execution log per draft
/// (`md_migration_log`), newest first. Surfaces the resume/revert checkpoints and
/// row counts the staged publish engine (ADR-0011) writes.
async fn migrations(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Query(q): Query<LimitQuery>,
) -> ApiResult<Json<Value>> {
    require_observability(&user)?;
    let limit = q.limit.unwrap_or(50).clamp(1, 500);

    // md_migration_log is RLS-gated (meta.*); read under the tenant GUC.
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, user.tenant_id).await?;

    let rows: Vec<MigrationRow> = sqlx::query_as(
        "SELECT mml.id, mml.draft_id, d.name AS draft_name,
                    mml.op, mml.status, mml.last_id, mml.rows_affected,
                    mml.started_at, mml.finished_at
               FROM meta.md_migration_log mml
               LEFT JOIN meta.md_draft d ON d.id = mml.draft_id
              WHERE mml.tenant_id = $1
              ORDER BY mml.started_at DESC, mml.id DESC
              LIMIT $2",
    )
    .bind(user.tenant_id)
    .bind(limit)
    .fetch_all(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;

    let items: Vec<Value> = rows
        .into_iter()
        .map(
            |(
                id,
                draft_id,
                draft_name,
                op,
                status,
                last_id,
                rows_affected,
                started_at,
                finished_at,
            )| {
                json!({
                    "id": id,
                    "draft_id": draft_id,
                    "draft_name": draft_name,
                    "op": op,
                    "status": status,
                    "last_id": last_id,
                    "rows_affected": rows_affected,
                    "started_at": started_at,
                    "finished_at": finished_at,
                })
            },
        )
        .collect();
    Ok(Json(json!({ "items": items })))
}

#[derive(Deserialize, Default)]
struct AuditQuery {
    #[serde(default)]
    entity: Option<String>,
    #[serde(default)]
    record_id: Option<Uuid>,
    #[serde(default)]
    op: Option<String>,
    #[serde(default)]
    actor_id: Option<Uuid>,
    #[serde(default)]
    limit: Option<i64>,
}

/// `GET /api/observability/audit` — browse the compliance audit trail. The
/// `before`/`after` snapshots are returned verbatim (the console is
/// superuser-only; see module docs).
async fn audit(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Query(q): Query<AuditQuery>,
) -> ApiResult<Json<Value>> {
    require_observability(&user)?;
    let limit = q.limit.unwrap_or(100).clamp(1, 500);
    // Non-superuser observability readers see the trail (who/what/when) but not
    // the field-level before/after payloads (may carry sensitive values).
    let redact = !user.is_superuser;

    let rows: Vec<AuditRow> = sqlx::query_as(
        "SELECT id, created_at, actor_id, entity, record_id, op, before, after
               FROM sys_audit_log
              WHERE tenant_id = $1
                AND ($2::text   IS NULL OR entity    = $2)
                AND ($3::uuid    IS NULL OR record_id = $3)
                AND ($4::text    IS NULL OR op        = $4)
                AND ($5::uuid    IS NULL OR actor_id  = $5)
              ORDER BY created_at DESC, id DESC
              LIMIT $6",
    )
    .bind(user.tenant_id)
    .bind(&q.entity)
    .bind(q.record_id)
    .bind(&q.op)
    .bind(q.actor_id)
    .bind(limit)
    .fetch_all(&st.pool)
    .await
    .map_err(Error::internal)?;

    let items: Vec<Value> = rows
        .into_iter()
        .map(|(id, at, actor_id, entity, record_id, op, before, after)| {
            json!({
                "id": id,
                "at": at,
                "actor_id": actor_id,
                "entity": entity,
                "record_id": record_id,
                "op": op,
                "before": if redact { None } else { before },
                "after": if redact { None } else { after },
            })
        })
        .collect();
    Ok(Json(json!({ "items": items })))
}

#[derive(Deserialize, Default)]
struct LimitQuery {
    #[serde(default)]
    limit: Option<i64>,
}

/// Gate the console to operators/modelers. A superuser always passes; a
/// non-admin principal passes when granted the `observability.read` capability
/// (a `("*", "observability.read")` permission). Non-superuser readers get
/// audit `before`/`after` redacted (field-level projection; ADR-0018 follow-up).
fn require_observability(user: &mda_security::Identity) -> ApiResult<()> {
    if user.is_superuser || user.can("*", "observability.read") {
        Ok(())
    } else {
        Err(Error::Forbidden(
            "observability console requires the observability.read capability".into(),
        )
        .into())
    }
}

/// `require_admin` is retained as an alias for callers that semantically want
/// the modeler/operator gate (same check today).
#[allow(dead_code)]
fn require_admin(user: &mda_security::Identity) -> ApiResult<()> {
    require_observability(user)
}
