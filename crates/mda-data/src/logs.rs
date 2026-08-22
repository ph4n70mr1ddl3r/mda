//! Write-path logging (PLAN §5.9.3 step 7): `sys_audit_log` (the compliance
//! record — full before/after) and `sys_event_log` (the lightweight facts —
//! push/replay) are written **in the same transaction as the data write**, so
//! a committed write always carries its logs and a rolled-back write leaves
//! neither. Shared by the REST/GraphQL services (`mda-api`) and the workflow
//! engine (`mda-workflow`) — every writer funnels through here.

use serde_json::{json, Value};
use uuid::Uuid;

use mda_core::{Error, Result};

/// Write the audit row + (when the op maps to a domain event) the event row on
/// the caller's transaction. Runs under the tenant GUC the caller set.
#[allow(clippy::too_many_arguments)]
pub async fn write_logs(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: Uuid,
    actor: Uuid,
    entity: &str,
    record_id: Option<Uuid>,
    op: &str,
    before: Option<Value>,
    after: Option<Value>,
) -> Result<()> {
    let (etype, payload) = event_for(op, &before, &after);
    sqlx::query(
        "INSERT INTO sys_audit_log (tenant_id, actor_id, entity, record_id, op, before, after)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(tenant)
    .bind(actor)
    .bind(entity)
    .bind(record_id.unwrap_or_else(Uuid::nil))
    .bind(op)
    .bind(&before)
    .bind(&after)
    .execute(&mut **tx)
    .await
    .map_err(Error::internal)?;
    if let (Some(etype), Some(rid)) = (etype, record_id) {
        sqlx::query(
            "INSERT INTO sys_event_log (tenant_id, type, entity, record_id, actor_id, payload)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(tenant)
        .bind(etype)
        .bind(entity)
        .bind(rid)
        .bind(actor)
        .bind(&payload)
        .execute(&mut **tx)
        .await
        .map_err(Error::internal)?;
    }
    Ok(())
}

/// Emit a non-CRUD domain event row on the caller's transaction (e.g.
/// `workflow.transitioned`, §5.10.2) — same txn as the state change it
/// describes.
pub async fn write_event(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: Uuid,
    actor: Uuid,
    entity: &str,
    record_id: Uuid,
    etype: &str,
    payload: Value,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO sys_event_log (tenant_id, type, entity, record_id, actor_id, payload)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(tenant)
    .bind(etype)
    .bind(entity)
    .bind(record_id)
    .bind(actor)
    .bind(&payload)
    .execute(&mut **tx)
    .await
    .map_err(Error::internal)?;
    Ok(())
}

/// Map an audit op to a domain event type + a conservative payload. The
/// payload carries only field *names* that changed (never values) so the SSE
/// relay can notify a viewer that a record changed without leaking
/// field-level data (§5.10.6 field-grain filtering is a documented follow-up).
fn event_for(
    op: &str,
    before: &Option<Value>,
    after: &Option<Value>,
) -> (Option<&'static str>, Value) {
    let changed: Vec<String> = match (before, after) {
        (Some(Value::Object(b)), Some(Value::Object(a))) => {
            let core: &[&str] = &["id", "version", "updated_at", "created_at"];
            a.iter()
                .filter(|(k, v)| !core.contains(&k.as_str()) && b.get(*k) != Some(*v))
                .map(|(k, _)| k.clone())
                .collect()
        }
        (_, Some(Value::Object(a))) => {
            let core: &[&str] = &["id", "version", "updated_at", "created_at"];
            a.keys()
                .filter(|k| !core.contains(&k.as_str()))
                .cloned()
                .collect()
        }
        _ => Vec::new(),
    };
    let (etype, from_v, to_v) = match op {
        "create" => (
            Some("record.created"),
            None,
            after.as_ref().and_then(version_of),
        ),
        "update" => (
            Some("record.updated"),
            before.as_ref().and_then(version_of),
            after.as_ref().and_then(version_of),
        ),
        "delete" => (
            Some("record.deleted"),
            before.as_ref().and_then(version_of),
            None,
        ),
        _ => (None, None, None),
    };
    let payload = json!({
        "changed_fields": changed,
        "from_version": from_v,
        "to_version": to_v,
    });
    (etype, payload)
}

fn version_of(v: &Value) -> Option<i64> {
    v.get("version").and_then(|x| x.as_i64())
}
