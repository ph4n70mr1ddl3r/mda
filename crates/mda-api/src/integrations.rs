//! Integration API (PLAN §5.22 / Phase 9): author connectors + flows and
//! manage/run them. The hub model — flows materialize external data into the
//! canonical `biz.*` entities; correlation is via `int_external_id`.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use mda_core::Error;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::error::ApiResult;
use crate::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/connectors",
            post(create_connector).get(list_connectors),
        )
        .route("/api/flows", post(create_flow).get(list_flows))
        .route("/api/flows/:id", get(get_flow))
        .route("/api/flows/:id/run", post(run_flow))
        .route("/api/flows/:id/runs", get(list_runs))
        .route("/api/external-ids/:entity/:key", get(lookup_external_id))
}

// ===== connectors =====

#[derive(Debug, Deserialize)]
struct CreateConnector {
    name: String,
    #[serde(default = "default_http")]
    transport: String,
    base_url: String,
    #[serde(default = "default_none_auth")]
    auth: Value,
}
fn default_http() -> String {
    "http".to_string()
}
fn default_none_auth() -> Value {
    json!({"kind":"none"})
}

async fn create_connector(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Json(body): Json<CreateConnector>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    crate::admin::require_admin(&user)?;
    if body.name.trim().is_empty() || body.base_url.trim().is_empty() {
        return Err(Error::Invalid("name and base_url are required".into()).into());
    }
    // SSRF guard: connector targets are operator config, but a tenant admin
    // account must not be able to aim the platform at internal/metadata
    // endpoints (re-checked at every fetch/push).
    let target = mda_integration::net::parse_outbound_url(&body.base_url)?;
    mda_integration::net::assert_public_egress(&target).await?;
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, user.tenant_id).await?;
    let row: Option<(Uuid,)> = sqlx::query_as(
        "INSERT INTO int.connector (tenant_id, name, transport, base_url, auth)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (tenant_id, name) DO NOTHING RETURNING id",
    )
    .bind(user.tenant_id)
    .bind(&body.name)
    .bind(&body.transport)
    .bind(&body.base_url)
    .bind(&body.auth)
    .fetch_optional(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    let (id,) = row.ok_or_else(|| Error::Conflict(format!("connector {} exists", body.name)))?;
    Ok((
        StatusCode::CREATED,
        Json(
            json!({"id": id, "name": body.name, "transport": body.transport, "base_url": body.base_url, "auth": body.auth}),
        ),
    ))
}

async fn list_connectors(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
) -> ApiResult<Json<Vec<Value>>> {
    crate::admin::require_admin(&user)?;
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, user.tenant_id).await?;
    let rows: Vec<(Value,)> = sqlx::query_as(
        "SELECT to_jsonb(c.*) FROM int.connector c WHERE tenant_id = $1 ORDER BY name",
    )
    .bind(user.tenant_id)
    .fetch_all(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(Json(rows.into_iter().map(|(v,)| v).collect()))
}

// ===== flows =====

#[derive(Debug, Deserialize)]
struct CreateFlow {
    name: String,
    direction: String,
    entity: String,
    connector_id: Option<Uuid>,
    webhook_id: Option<Uuid>,
    endpoint_path: Option<String>,
    #[serde(default = "default_empty_obj")]
    mapping: Value,
    #[serde(default = "default_ext_key")]
    external_key_field: String,
    #[serde(default = "default_lww")]
    conflict_policy: String,
    system: Option<String>,
    /// Per-flow scoped principal: newly created records are owned by this user
    /// instead of a blanket system superuser (§5.22 follow-up).
    #[serde(default)]
    running_user_id: Option<Uuid>,
    /// Flow-level config (e.g. `sor_fields` for the `field_level_sor` policy).
    #[serde(default = "default_empty_obj")]
    config: Value,
}
fn default_empty_obj() -> Value {
    json!({})
}
fn default_ext_key() -> String {
    "external_id".to_string()
}
fn default_lww() -> String {
    "last_write_wins".to_string()
}

async fn create_flow(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Json(body): Json<CreateFlow>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    crate::admin::require_admin(&user)?;
    if !matches!(body.direction.as_str(), "inbound" | "outbound") {
        return Err(Error::Invalid("direction must be inbound|outbound".into()).into());
    }
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, user.tenant_id).await?;
    let row: Option<(Uuid,)> = sqlx::query_as(
        "INSERT INTO int.flow
            (tenant_id, name, direction, entity, connector_id, webhook_id, endpoint_path,
             mapping, external_key_field, conflict_policy, system, running_user_id, config)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
         ON CONFLICT (tenant_id, name) DO NOTHING RETURNING id",
    )
    .bind(user.tenant_id)
    .bind(&body.name)
    .bind(&body.direction)
    .bind(&body.entity)
    .bind(body.connector_id)
    .bind(body.webhook_id)
    .bind(&body.endpoint_path)
    .bind(&body.mapping)
    .bind(&body.external_key_field)
    .bind(&body.conflict_policy)
    .bind(&body.system)
    .bind(body.running_user_id)
    .bind(&body.config)
    .fetch_optional(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    let (id,) = row.ok_or_else(|| Error::Conflict(format!("flow {} exists", body.name)))?;
    Ok((
        StatusCode::CREATED,
        Json(
            json!({"id": id, "name": body.name, "direction": body.direction, "entity": body.entity}),
        ),
    ))
}

async fn list_flows(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
) -> ApiResult<Json<Vec<Value>>> {
    crate::admin::require_admin(&user)?;
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    mda_security::set_tenant(&mut tx, user.tenant_id).await?;
    let rows: Vec<(Value,)> =
        sqlx::query_as("SELECT to_jsonb(f.*) FROM int.flow f WHERE tenant_id = $1 ORDER BY name")
            .bind(user.tenant_id)
            .fetch_all(&mut *tx)
            .await
            .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(Json(rows.into_iter().map(|(v,)| v).collect()))
}

async fn get_flow(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    crate::admin::require_admin(&user)?;
    let flow = mda_integration::flow_by_id(&st.pool, user.tenant_id, id).await?;
    Ok(Json(json!({
        "id": flow.id, "name": flow.name, "direction": flow.direction, "entity": flow.entity,
        "connector_id": flow.connector_id, "webhook_id": flow.webhook_id,
        "endpoint_path": flow.endpoint_path, "mapping": flow.mapping,
        "external_key_field": flow.external_key_field, "conflict_policy": flow.conflict_policy,
        "system": flow.system,
    })))
}

/// `POST /api/flows/:id/run` — manually run a flow.
/// - inbound: body is the external payload (or `{ "payload": {...} }`); materializes.
/// - outbound: body is the biz record to push (or `{ "record": {...} }`).
#[derive(Debug, Deserialize)]
struct RunBody {
    #[serde(default)]
    payload: Option<Value>,
    #[serde(default)]
    record: Option<Value>,
}

async fn run_flow(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<RunBody>,
) -> ApiResult<Json<Value>> {
    // Admin-gated: a manual run materializes data under the system write path
    // (superuser record scope), so it must not be reachable by unprivileged
    // users — the API-layer RBAC/FLS checks of /api/data don't apply here.
    crate::admin::require_admin(&user)?;
    let flow = mda_integration::flow_by_id(&st.pool, user.tenant_id, id).await?;
    // resolve the entity definition (int flows target a canonical biz entity).
    let entity_id =
        mda_meta::loader::entity_id_by_name(&st.pool, user.tenant_id, &flow.entity).await?;
    let def = mda_meta::loader::load_entity_definition(&st.pool, user.tenant_id, entity_id).await?;
    match flow.direction.as_str() {
        "inbound" => {
            let external = body
                .payload
                .ok_or_else(|| Error::Invalid("inbound run needs {\"payload\":{...}}".into()))?;
            let ids =
                mda_integration::run_inbound_batch(&st.pool, &def, &flow, &external, user.user_id)
                    .await?;
            Ok(Json(json!({"record_ids": ids})))
        }
        "outbound" => {
            let record = body
                .record
                .ok_or_else(|| Error::Invalid("outbound run needs {\"record\":{...}}".into()))?;
            mda_integration::run_outbound(&st.pool, st.secrets.as_ref(), &flow, &record).await?;
            Ok(Json(json!({"pushed": true})))
        }
        other => Err(Error::Invalid(format!("unknown direction {other}")).into()),
    }
}

async fn list_runs(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Vec<Value>>> {
    crate::admin::require_admin(&user)?;
    let rows: Vec<(Value,)> = sqlx::query_as(
        "SELECT to_jsonb(r.*) FROM sys_integration_run r
          WHERE tenant_id = $1 AND flow_id = $2 ORDER BY started_at DESC LIMIT 50",
    )
    .bind(user.tenant_id)
    .bind(id)
    .fetch_all(&st.pool)
    .await
    .map_err(Error::internal)?;
    Ok(Json(rows.into_iter().map(|(v,)| v).collect()))
}

// ===== external-ID registry =====

#[derive(Debug, Deserialize)]
struct ExtIdQuery {
    system: Option<String>,
}

/// `GET /api/external-ids/:entity/:key[?system=]` — resolve a platform record by
/// its external key (the correlation registry, §5.22.3).
async fn lookup_external_id(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path((entity, key)): Path<(String, String)>,
    Query(q): Query<ExtIdQuery>,
) -> ApiResult<Json<Value>> {
    crate::admin::require_admin(&user)?;
    let row: Option<(Uuid,)> = match q.system.as_deref() {
        Some(system) => sqlx::query_as(
            "SELECT record_id FROM int_external_id
              WHERE tenant_id = $1 AND entity = $2 AND external_key = $3 AND system = $4",
        )
        .bind(user.tenant_id)
        .bind(&entity)
        .bind(&key)
        .bind(system)
        .fetch_optional(&st.pool)
        .await
        .map_err(Error::internal)?,
        None => sqlx::query_as(
            "SELECT record_id FROM int_external_id
              WHERE tenant_id = $1 AND entity = $2 AND external_key = $3",
        )
        .bind(user.tenant_id)
        .bind(&entity)
        .bind(&key)
        .fetch_optional(&st.pool)
        .await
        .map_err(Error::internal)?,
    };
    let (record_id,) = row.ok_or_else(|| Error::NotFound(format!("external id {entity}/{key}")))?;
    Ok(Json(
        json!({"entity": entity, "external_key": key, "record_id": record_id}),
    ))
}
