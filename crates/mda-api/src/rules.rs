//! Business-rules authoring API (PLAN Phase 8 / §4.3): the Studio's rule
//! editor surface over `meta.md_rule`. Until now rules could only be authored
//! by direct DB access; this makes the Phase-4 engine operable from the
//! browser with author-time validation (entity/event/field resolve against the
//! active model; condition and value parse as bounded-DSL expressions) so a
//! typo fails at author time, not on every write.
//!
//! Superuser-gated like the rest of the Studio surfaces: rules run in every
//! write transaction, so authoring them is as sensitive as the security graph.

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

/// Events the Phase-4 engine fires on (mda-rules `fire` is called per event).
const EVENTS: &[&str] = &[
    "before_create",
    "before_update",
    "after_create",
    "after_update",
];

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/rules", get(list_rules).post(create_rule))
        .route(
            "/api/rules/:id",
            axum::routing::patch(update_rule).delete(delete_rule),
        )
}

fn require_studio(id: &mda_security::Identity) -> ApiResult<()> {
    if !id.is_superuser {
        return Err(Error::Forbidden("rule authoring requires an admin role".into()).into());
    }
    Ok(())
}

#[derive(sqlx::FromRow, serde::Serialize)]
struct RuleRow {
    id: Uuid,
    entity: String,
    event: String,
    condition: Value,
    action_type: String,
    action_field: Option<String>,
    action_value: Option<Value>,
    active: bool,
    priority: i32,
    created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Deserialize)]
struct CreateRule {
    entity: String,
    event: String,
    #[serde(default = "default_condition")]
    condition: Value,
    action_field: String,
    action_value: Value,
    #[serde(default = "default_priority")]
    priority: i32,
    #[serde(default = "default_true")]
    active: bool,
}

fn default_condition() -> Value {
    serde_json::json!({"op": "Lit", "value": true})
}

fn default_priority() -> i32 {
    100
}

fn default_true() -> bool {
    true
}

#[derive(Deserialize, Default)]
struct UpdateRule {
    #[serde(default)]
    condition: Option<Value>,
    #[serde(default)]
    action_field: Option<String>,
    #[serde(default)]
    action_value: Option<Value>,
    #[serde(default)]
    priority: Option<i32>,
    #[serde(default)]
    active: Option<bool>,
}

/// `GET /api/rules` — the tenant's rules, execution order (priority, id).
async fn list_rules(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
) -> ApiResult<Json<Vec<RuleRow>>> {
    require_studio(&user)?;
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    let rows: Vec<RuleRow> = sqlx::query_as(
        "SELECT id, entity, event, condition, action_type, action_field, action_value, active, priority, created_at \
         FROM meta.md_rule WHERE tenant_id = $1 ORDER BY entity, priority, id",
    )
    .bind(user.tenant_id)
    .fetch_all(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(Json(rows))
}

/// Author-time validation shared by create + update: the entity exists in the
/// active model, the event is one the engine fires, the target field resolves,
/// and condition/action-value parse as bounded-DSL expressions.
async fn validate_shape(
    st: &AppState,
    tenant: Uuid,
    entity: &str,
    event: &str,
    condition: &Value,
    action_field: &str,
    action_value: &Value,
) -> ApiResult<()> {
    if !EVENTS.contains(&event) {
        return Err(Error::Invalid(format!("event must be one of: {}", EVENTS.join(", "))).into());
    }
    let entity_id = mda_meta::loader::entity_id_by_name(&st.pool, tenant, entity)
        .await
        .map_err(|_| Error::Invalid(format!("unknown entity {entity}")))?;
    let def = mda_meta::loader::load_entity_definition(&st.pool, tenant, entity_id).await?;
    if !def.fields.iter().any(|f| f.name == action_field) {
        return Err(Error::Invalid(format!("unknown action field {entity}.{action_field}")).into());
    }
    parse_expr(condition, "condition")?;
    parse_expr(action_value, "action_value")?;
    Ok(())
}

fn parse_expr(v: &Value, what: &str) -> ApiResult<()> {
    serde_json::from_value::<mda_expression::Expr>(v.clone())
        .map_err(|e| Error::Invalid(format!("{what} is not a valid expression: {e}")).into())
        .map(|_| ())
}

/// `POST /api/rules` — author a set-field rule.
async fn create_rule(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Json(body): Json<CreateRule>,
) -> ApiResult<(StatusCode, Json<RuleRow>)> {
    require_studio(&user)?;
    if body.action_field.trim().is_empty() {
        return Err(Error::Invalid("action_field is required".into()).into());
    }
    validate_shape(
        &st,
        user.tenant_id,
        &body.entity,
        &body.event,
        &body.condition,
        &body.action_field,
        &body.action_value,
    )
    .await?;
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    let row: RuleRow = sqlx::query_as(
        "INSERT INTO meta.md_rule \
             (tenant_id, entity, event, condition, action_type, action_field, action_value, active, priority) \
         VALUES ($1, $2, $3, $4, 'set_field', $5, $6, $7, $8) \
         RETURNING id, entity, event, condition, action_type, action_field, action_value, active, priority, created_at",
    )
    .bind(user.tenant_id)
    .bind(&body.entity)
    .bind(&body.event)
    .bind(&body.condition)
    .bind(&body.action_field)
    .bind(&body.action_value)
    .bind(body.active)
    .bind(body.priority)
    .fetch_one(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok((StatusCode::CREATED, Json(row)))
}

/// `PATCH /api/rules/:id` — edit condition / action / priority / active. The
/// entity and event are fixed at create (a rule for another event is a new
/// rule), so validation only needs the stored values for the rest.
async fn update_rule(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateRule>,
) -> ApiResult<Json<RuleRow>> {
    require_studio(&user)?;
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    let existing: RuleRow = sqlx::query_as(
        "SELECT id, entity, event, condition, action_type, action_field, action_value, active, priority, created_at \
         FROM meta.md_rule WHERE tenant_id = $1 AND id = $2",
    )
    .bind(user.tenant_id)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(Error::internal)?
    .ok_or_else(|| Error::NotFound(format!("rule {id}")))?;
    tx.commit().await.map_err(Error::internal)?;

    let condition = body.condition.unwrap_or(existing.condition.clone());
    let action_field = body
        .action_field
        .unwrap_or_else(|| existing.action_field.clone().unwrap_or_default());
    let action_value = body.action_value.or_else(|| existing.action_value.clone());
    let Some(action_value) = action_value else {
        return Err(Error::Invalid("action_value is required".into()).into());
    };
    let priority = body.priority.unwrap_or(existing.priority);
    let active = body.active.unwrap_or(existing.active);
    validate_shape(
        &st,
        user.tenant_id,
        &existing.entity,
        &existing.event,
        &condition,
        &action_field,
        &action_value,
    )
    .await?;

    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    let row: RuleRow = sqlx::query_as(
        "UPDATE meta.md_rule SET condition = $3, action_field = $4, action_value = $5, priority = $6, active = $7 \
         WHERE tenant_id = $1 AND id = $2 \
         RETURNING id, entity, event, condition, action_type, action_field, action_value, active, priority, created_at",
    )
    .bind(user.tenant_id)
    .bind(id)
    .bind(&condition)
    .bind(&action_field)
    .bind(&action_value)
    .bind(priority)
    .bind(active)
    .fetch_one(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(Json(row))
}

/// `DELETE /api/rules/:id`
async fn delete_rule(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    require_studio(&user)?;
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    let n = sqlx::query("DELETE FROM meta.md_rule WHERE tenant_id = $1 AND id = $2")
        .bind(user.tenant_id)
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(Error::internal)?
        .rows_affected();
    tx.commit().await.map_err(Error::internal)?;
    if n == 0 {
        Err(Error::NotFound(format!("rule {id}")).into())
    } else {
        Ok(StatusCode::NO_CONTENT)
    }
}
