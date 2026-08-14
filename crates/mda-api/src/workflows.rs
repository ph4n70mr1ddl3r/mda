//! Workflow authoring API (PLAN Phase 8 / §4.3): the Studio's workflow
//! designer surface over `meta.md_workflow{,_state,_transition}`. The Phase-5
//! engine executes what these tables declare; until now they could only be
//! populated by direct DB access. Author-time validation checks the whole
//! machine: the entity exists, states are unique, every transition connects
//! two declared states, guards parse as bounded-DSL expressions, and action
//! fields resolve against the entity — so a malformed machine can't be saved.
//!
//! The engine runs ONE active workflow per entity (`run_transition` picks the
//! active one by entity), which the uniqueness constraint on
//! `(tenant, entity, name)` plus the `active` flag encode; creating a second
//! machine for an entity is allowed but only one may be active.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use mda_core::Error;
use mda_security::set_tenant;
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::error::ApiResult;
use crate::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/workflows", get(list_workflows).post(create_workflow))
        .route(
            "/api/workflows/:id",
            axum::routing::patch(update_workflow).delete(delete_workflow),
        )
}

fn require_studio(id: &mda_security::Identity) -> ApiResult<()> {
    if !id.is_superuser {
        return Err(Error::Forbidden("workflow authoring requires an admin role".into()).into());
    }
    Ok(())
}

#[derive(sqlx::FromRow, serde::Serialize)]
struct WorkflowRow {
    id: Uuid,
    entity: String,
    name: String,
    active: bool,
    created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Deserialize)]
struct TransitionBody {
    name: String,
    from_state: String,
    to_state: String,
    #[serde(default = "default_guard")]
    guard: Value,
    #[serde(default)]
    actions: Vec<ActionBody>,
    #[serde(default)]
    creates_task: bool,
}

#[derive(Deserialize)]
struct ActionBody {
    field: String,
    value: Value,
}

fn default_guard() -> Value {
    serde_json::json!({"op": "Lit", "value": true})
}

#[derive(Deserialize)]
struct CreateWorkflow {
    entity: String,
    name: String,
    #[serde(default)]
    states: Vec<String>,
    #[serde(default)]
    transitions: Vec<TransitionBody>,
    #[serde(default = "default_true")]
    active: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Deserialize, Default)]
struct UpdateWorkflow {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    active: Option<bool>,
}

/// `GET /api/workflows` — every machine with its states and transitions
/// (nested), so the designer can render the graph as authored.
async fn list_workflows(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
) -> ApiResult<Json<Vec<Value>>> {
    require_studio(&user)?;
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    let wfs: Vec<WorkflowRow> = sqlx::query_as(
        "SELECT id, entity, name, active, created_at FROM meta.md_workflow \
         WHERE tenant_id = $1 ORDER BY entity, name",
    )
    .bind(user.tenant_id)
    .fetch_all(&mut *tx)
    .await
    .map_err(Error::internal)?;
    let mut out = Vec::with_capacity(wfs.len());
    for wf in &wfs {
        let states: Vec<(Uuid, String)> = sqlx::query_as(
            "SELECT id, name FROM meta.md_workflow_state WHERE workflow_id = $1 ORDER BY name",
        )
        .bind(wf.id)
        .fetch_all(&mut *tx)
        .await
        .map_err(Error::internal)?;
        let transitions: Vec<(Uuid, String, String, String, Value, Value, bool)> = sqlx::query_as(
            "SELECT id, name, from_state, to_state, guard, actions, creates_task \
             FROM meta.md_workflow_transition WHERE workflow_id = $1 ORDER BY name",
        )
        .bind(wf.id)
        .fetch_all(&mut *tx)
        .await
        .map_err(Error::internal)?;
        out.push(serde_json::json!({
            "id": wf.id,
            "entity": wf.entity,
            "name": wf.name,
            "active": wf.active,
            "created_at": wf.created_at,
            "states": states.into_iter().map(|(_, n)| n).collect::<Vec<_>>(),
            "transitions": transitions.into_iter().map(|(id, name, from, to, guard, actions, creates_task)| {
                serde_json::json!({
                    "id": id, "name": name, "from_state": from, "to_state": to,
                    "guard": guard, "actions": actions, "creates_task": creates_task,
                })
            }).collect::<Vec<_>>(),
        }));
    }
    tx.commit().await.map_err(Error::internal)?;
    Ok(Json(out))
}

/// `POST /api/workflows` — author a full machine (states + transitions) in one
/// transaction: either the whole graph is valid or nothing is stored.
async fn create_workflow(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Json(body): Json<CreateWorkflow>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    require_studio(&user)?;
    if body.name.trim().is_empty() {
        return Err(Error::Invalid("workflow name is required".into()).into());
    }
    validate_machine(
        &st,
        user.tenant_id,
        &body.entity,
        &body.states,
        &body.transitions,
    )
    .await?;

    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    let (wf_id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO meta.md_workflow (tenant_id, entity, name, active) VALUES ($1, $2, $3, $4) \
         RETURNING id",
    )
    .bind(user.tenant_id)
    .bind(&body.entity)
    .bind(&body.name)
    .bind(body.active)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| match e {
        sqlx::Error::Database(ref d) if d.is_unique_violation() => {
            Error::Conflict("workflow name already exists for this entity".into())
        }
        other => Error::internal(other),
    })?;
    for s in &body.states {
        sqlx::query("INSERT INTO meta.md_workflow_state (workflow_id, name) VALUES ($1, $2)")
            .bind(wf_id)
            .bind(s)
            .execute(&mut *tx)
            .await
            .map_err(Error::internal)?;
    }
    for t in &body.transitions {
        let actions = serde_json::to_value(
            t.actions
                .iter()
                .map(|a| serde_json::json!({"field": a.field, "value": a.value}))
                .collect::<Vec<_>>(),
        )
        .map_err(Error::internal)?;
        sqlx::query(
            "INSERT INTO meta.md_workflow_transition \
                 (workflow_id, name, from_state, to_state, guard, actions, creates_task) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(wf_id)
        .bind(&t.name)
        .bind(&t.from_state)
        .bind(&t.to_state)
        .bind(&t.guard)
        .bind(&actions)
        .bind(t.creates_task)
        .execute(&mut *tx)
        .await
        .map_err(Error::internal)?;
    }
    tx.commit().await.map_err(Error::internal)?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({"id": wf_id, "entity": body.entity, "name": body.name})),
    ))
}

/// Whole-machine shape check: entity + states + transitions + expressions.
async fn validate_machine(
    st: &AppState,
    tenant: Uuid,
    entity: &str,
    states: &[String],
    transitions: &[TransitionBody],
) -> ApiResult<()> {
    let entity_id = mda_meta::loader::entity_id_by_name(&st.pool, tenant, entity)
        .await
        .map_err(|_| Error::Invalid(format!("unknown entity {entity}")))?;
    let def = mda_meta::loader::load_entity_definition(&st.pool, tenant, entity_id).await?;
    if def.entity.status != "active" {
        return Err(Error::Invalid(format!("entity {entity} is retired")).into());
    }
    if states.is_empty() {
        return Err(Error::Invalid("a workflow needs at least one state".into()).into());
    }
    let mut seen = std::collections::HashSet::new();
    for s in states {
        if s.trim().is_empty() {
            return Err(Error::Invalid("state names must not be empty".into()).into());
        }
        if !seen.insert(s.clone()) {
            return Err(Error::Invalid(format!("duplicate state '{s}'")).into());
        }
    }
    for t in transitions {
        if !states.contains(&t.from_state) || !states.contains(&t.to_state) {
            return Err(Error::Invalid(format!(
                "transition '{}' connects states not in the machine ({} → {})",
                t.name, t.from_state, t.to_state
            ))
            .into());
        }
        serde_json::from_value::<mda_expression::Expr>(t.guard.clone()).map_err(|e| {
            Error::Invalid(format!(
                "guard of '{}' is not a valid expression: {e}",
                t.name
            ))
        })?;
        for a in &t.actions {
            if !def.fields.iter().any(|f| f.name == a.field) {
                return Err(Error::Invalid(format!(
                    "transition '{}' sets unknown field '{}'",
                    t.name, a.field
                ))
                .into());
            }
            serde_json::from_value::<mda_expression::Expr>(a.value.clone()).map_err(|e| {
                Error::Invalid(format!(
                    "action value of '{}'.'{}' is not a valid expression: {e}",
                    t.name, a.field
                ))
            })?;
        }
    }
    Ok(())
}

/// `PATCH /api/workflows/:id` — rename / (de)activate. Editing the graph is a
/// replace: delete + recreate (the machine is a unit; in-place transition
/// surgery invites half-edited machines).
async fn update_workflow(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateWorkflow>,
) -> ApiResult<Json<WorkflowRow>> {
    require_studio(&user)?;
    if let Some(ref name) = body.name {
        if name.trim().is_empty() {
            return Err(Error::Invalid("workflow name is required".into()).into());
        }
    }
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    let existing: WorkflowRow =
        sqlx::query_as("SELECT id, entity, name, active, created_at FROM meta.md_workflow WHERE tenant_id = $1 AND id = $2")
            .bind(user.tenant_id)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(Error::internal)?
            .ok_or_else(|| Error::NotFound(format!("workflow {id}")))?;
    let row: WorkflowRow = sqlx::query_as(
        "UPDATE meta.md_workflow SET name = $3, active = $4 WHERE tenant_id = $1 AND id = $2 \
         RETURNING id, entity, name, active, created_at",
    )
    .bind(user.tenant_id)
    .bind(id)
    .bind(body.name.unwrap_or(existing.name))
    .bind(body.active.unwrap_or(existing.active))
    .fetch_one(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(Json(row))
}

/// `DELETE /api/workflows/:id` — states, transitions and open tasks cascade
/// away with the machine (all three FK `ON DELETE CASCADE`).
async fn delete_workflow(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    require_studio(&user)?;
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    let n = sqlx::query("DELETE FROM meta.md_workflow WHERE tenant_id = $1 AND id = $2")
        .bind(user.tenant_id)
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(Error::internal)?
        .rows_affected();
    tx.commit().await.map_err(Error::internal)?;
    if n == 0 {
        Err(Error::NotFound(format!("workflow {id}")).into())
    } else {
        Ok(StatusCode::NO_CONTENT)
    }
}
