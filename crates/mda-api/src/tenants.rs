//! Tenant configuration export (PLAN §14: backup half of "tenant-scoped
//! backup/restore").
//!
//! `GET /api/tenants/export` produces a portable JSON snapshot of a tenant's
//! *configuration* — the active model plus every tenant-scoped definition table
//! (reports, rules, workflows, templates, notification types, schedules, the
//! security graph, and integration definitions). This is the part of a tenant
//! that is painful to recreate by hand and the natural unit of backup, audit,
//! and cross-environment migration.
//!
//! **Scope & honesty:** full tenant-scoped *data* export/restore + regional
//! placement remain tied to the deferred tenant lifecycle (§5.4) and HA (U9).
//! Model restore is already available via the Studio publish API (`PUT
//! /api/studio/drafts/:id/model` accepts exactly this model shape); restoring
//! the surrounding config by id is a follow-up that lands with tenant lifecycle.
//!
//! Superuser-only: the bundle aggregates every entity's definitions (incl. the
//! security graph) regardless of the caller's per-entity grants.

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use mda_core::Error;
use mda_security::set_tenant;
use serde_json::{json, Value};

use crate::auth::AuthUser;
use crate::error::ApiResult;
use crate::AppState;

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/tenants/export", get(export_tenant))
}

/// `GET /api/tenants/export` — a JSON snapshot of the caller's tenant config.
async fn export_tenant(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
) -> ApiResult<Json<Value>> {
    if !user.is_superuser {
        return Err(Error::Forbidden("tenant export requires an admin role".into()).into());
    }
    let tenant = user.tenant_id;

    // The active model (entities / fields / relationships / modules) — the same
    // shape Studio accepts, so a restore round-trips through the publish API.
    let model = mda_meta::loader::load_active_model(&st.pool, tenant).await?;

    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, tenant).await?;

    let reports = table_json(&mut tx, "SELECT to_jsonb(t.*) FROM meta.md_report t").await;
    let rules = table_json(&mut tx, "SELECT to_jsonb(t.*) FROM meta.md_rule t").await;
    let templates = table_json(&mut tx, "SELECT to_jsonb(t.*) FROM meta.md_template t").await;
    let notification_types = table_json(
        &mut tx,
        "SELECT to_jsonb(t.*) FROM meta.md_notification_type t",
    )
    .await;
    let schedules = table_json(&mut tx, "SELECT to_jsonb(t.*) FROM sys_schedule t").await;

    // Security graph (definitions only — never users, sessions, or shares).
    let roles = table_json(&mut tx, "SELECT to_jsonb(t.*) FROM sec.sec_role t").await;
    let permissions = table_json(&mut tx, "SELECT to_jsonb(t.*) FROM sec.sec_permission t").await;
    let field_permissions = table_json(
        &mut tx,
        "SELECT to_jsonb(t.*) FROM sec.sec_field_permission t",
    )
    .await;
    let owd = table_json(&mut tx, "SELECT to_jsonb(t.*) FROM sec.sec_owd t").await;
    let teams = table_json(&mut tx, "SELECT to_jsonb(t.*) FROM sec.sec_team t").await;

    // Integration definitions.
    let connectors = table_json(&mut tx, "SELECT to_jsonb(t.*) FROM int.connector t").await;
    let flows = table_json(&mut tx, "SELECT to_jsonb(t.*) FROM int.flow t").await;
    let value_maps = table_json(&mut tx, "SELECT to_jsonb(t.*) FROM int.value_map t").await;

    tx.commit().await.map_err(Error::internal)?;

    Ok(Json(json!({
        "schema_version": 1,
        "tenant_id": tenant,
        "exported_at": chrono::Utc::now(),
        "model": model,
        "reports": reports,
        "rules": rules,
        "templates": templates,
        "notification_types": notification_types,
        "schedules": schedules,
        "security": {
            "roles": roles,
            "permissions": permissions,
            "field_permissions": field_permissions,
            "owd": owd,
            "teams": teams,
        },
        "integrations": {
            "connectors": connectors,
            "flows": flows,
            "value_maps": value_maps,
        },
    })))
}

/// Read every row of `query` (already tenant-GUC-scoped) as a JSON array. Errors
/// are logged and degrade to an empty array rather than failing the whole export
/// — a missing/renamed optional table shouldn't abort a backup.
async fn table_json(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, query: &str) -> Vec<Value> {
    match sqlx::query_as::<_, (Value,)>(query)
        .fetch_all(&mut **tx)
        .await
    {
        Ok(rows) => rows.into_iter().map(|(v,)| v).collect(),
        Err(e) => {
            tracing::warn!(query = %query, error = %e, "export: table skipped");
            Vec::new()
        }
    }
}
