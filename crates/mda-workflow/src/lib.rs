//! `mda-workflow` — state-machine engine over entities (PLAN §4.3 / §5.9.5).
//!
//! A transition is a specialized update: the guard (a DSL expression) is
//! evaluated against the record; on success the record's `state` is advanced and
//! the transition's set-field actions are applied, a task may be created, and a
//! `workflow.transitioned` row is enqueued in the outbox — all in the write
//! transaction (ADR-0016 sync-by-default). Async timers (apalis) and the outbox
//! drain worker are follow-ups.

use mda_core::{Error, Result};
use mda_data::RecordScope;
use mda_expression::{eval, Expr, Registry};
use mda_meta::EntityDefinition;
use serde_json::{Map, Value};
use sqlx::PgPool;
use uuid::Uuid;

async fn set_tenant(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, tenant: Uuid) -> Result<()> {
    sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
        .bind(tenant.to_string())
        .execute(&mut **tx)
        .await
        .map_err(Error::internal)?;
    Ok(())
}

#[derive(sqlx::FromRow)]
struct Workflow {
    id: Uuid,
    #[allow(dead_code)]
    entity: String,
}

#[derive(sqlx::FromRow)]
struct Transition {
    id: Uuid,
    name: String,
    from_state: String,
    to_state: String,
    guard: Value,
    actions: Value,
    creates_task: bool,
}

/// Run a named transition on a record. Returns the updated record.
#[allow(clippy::too_many_arguments)]
pub async fn run_transition(
    pool: &PgPool,
    tenant: Uuid,
    actor: Uuid,
    def: &EntityDefinition,
    entity: &str,
    record_id: Uuid,
    transition_name: &str,
    expected_version: i64,
    scope: &RecordScope,
) -> Result<Value> {
    let mut tx = pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, tenant).await?;
    let wf: Option<Workflow> = sqlx::query_as(
        "SELECT id, entity FROM meta.md_workflow
          WHERE tenant_id = $1 AND entity = $2 AND active = TRUE LIMIT 1",
    )
    .bind(tenant)
    .bind(entity)
    .fetch_optional(&mut *tx)
    .await
    .map_err(Error::internal)?;
    let wf = wf.ok_or_else(|| Error::NotFound(format!("no workflow for {entity}")))?;

    // Visibility check first, with the caller's real scope: reading with the
    // superuser scope before the scope check would let error messages ("no
    // transition X from state Y", "guard failed") disclose the current state of
    // records the caller cannot even read.
    mda_data::read(pool, tenant, def, record_id, scope).await?;
    let current =
        mda_data::read(pool, tenant, def, record_id, &RecordScope::superuser(actor)).await?;
    let state = current["state"]
        .as_str()
        .ok_or_else(|| Error::Internal(anyhow::anyhow!("record has no state")))?
        .to_string();

    let transitions: Vec<Transition> = sqlx::query_as(
        "SELECT id, name, from_state, to_state, guard, actions, creates_task
           FROM meta.md_workflow_transition WHERE workflow_id = $1",
    )
    .bind(wf.id)
    .fetch_all(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    let t = transitions
        .iter()
        .find(|t| t.name == transition_name && t.from_state == state)
        .ok_or_else(|| {
            Error::Invalid(format!(
                "no transition '{transition_name}' from state '{state}'"
            ))
        })?;

    // build the condition/action context from the current record (data fields only)
    let mut ctx = record_ctx(&current);
    let reg = Registry::new();

    // guard
    let guard = Expr::from_json(&t.guard)?;
    if !mda_expression::truth(&eval(&guard, &Value::Object(ctx.clone()), &reg)?) {
        return Err(Error::Invalid(format!(
            "transition '{transition_name}' guard failed"
        )));
    }

    // apply transition actions (set_field), then fire after_update rules + calculated
    for action in t.actions.as_array().unwrap_or(&Vec::new()) {
        let field = action.get("field").and_then(|v| v.as_str());
        let value_expr = action.get("value");
        if let (Some(field), Some(value_expr)) = (field, value_expr) {
            let v = eval(
                &Expr::from_json(value_expr)?,
                &Value::Object(ctx.clone()),
                &reg,
            )?;
            ctx.insert(field.to_string(), v);
        }
    }
    ctx.insert("_new_state".to_string(), Value::String(t.to_state.clone()));
    let rules = mda_rules::load_active(pool, tenant, entity).await?;
    mda_rules::fire(&rules, "after_update", &mut ctx, &reg)?;
    mda_rules::compute_calculated(def, &mut ctx, &reg)?;
    ctx.remove("_new_state");

    // persist: set state + derived fields (OCC + write scope)
    let after = mda_data::update(
        pool,
        tenant,
        def,
        record_id,
        expected_version,
        ctx,
        scope,
        Some(t.to_state.clone()),
    )
    .await?;

    // create a task if the transition requires one
    if t.creates_task {
        let mut tx = pool.begin().await.map_err(Error::internal)?;
        set_tenant(&mut tx, tenant).await?;
        sqlx::query(
            "INSERT INTO meta.md_workflow_task (tenant_id, workflow_id, entity, record_id, transition_id)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(tenant)
        .bind(wf.id)
        .bind(entity)
        .bind(record_id)
        .bind(t.id)
        .execute(&mut *tx)
        .await
        .map_err(Error::internal)?;
        tx.commit().await.map_err(Error::internal)?;
    }

    // enqueue a side-effect (outbox); the drain worker turns the
    // `workflow.transitioned` row into an in-app notification to the actor.
    sqlx::query(
        "INSERT INTO sys_outbox (tenant_id, kind, payload)
         VALUES ($1, 'workflow.transitioned', $2)",
    )
    .bind(tenant)
    .bind(serde_json::json!({
        "entity": entity,
        "record_id": record_id,
        "transition": transition_name,
        "from": state,
        "to": t.to_state,
        "actor": actor,
    }))
    .execute(pool)
    .await
    .map_err(Error::internal)?;

    Ok(after)
}

/// Extract the data fields (non-core) from a record value into a map.
fn record_ctx(rec: &Value) -> Map<String, Value> {
    let mut m = Map::new();
    if let Some(obj) = rec.as_object() {
        for (k, v) in obj {
            if matches!(
                k.as_str(),
                "id" | "version" | "owner_id" | "state" | "created_at" | "updated_at"
            ) {
                continue;
            }
            m.insert(k.clone(), v.clone());
        }
    }
    m
}
