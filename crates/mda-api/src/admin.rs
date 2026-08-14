//! Admin security-graph API (PLAN §5.11): a superuser-only management surface
//! for the tenant's security graph — teams (incl. the ADR-0013 `parent_id`
//! hierarchy), roles, object/field permissions, org-wide defaults, role
//! assignments, and users. Until now this graph was only editable through the
//! DB or the tenant-config import; this surface makes it operable for a real
//! operator and makes the team hierarchy actually usable.
//!
//! Every handler is superuser-gated (the `("*","*")` role): the security graph
//! is the trust root, so only an admin may reshape it. All writes run under the
//! tenant GUC so the `sec.*` RLS policies engage (fail-closed without it).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use mda_core::Error;
use mda_security::{hash_password, set_tenant};
use serde::{Deserialize, Deserializer, Serialize};
use uuid::Uuid;

use crate::auth::AuthUser;
use crate::error::ApiResult;
use crate::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        // ---- teams (incl. parent_id hierarchy) ----
        .route("/api/admin/teams", get(list_teams).post(create_team))
        .route(
            "/api/admin/teams/:id",
            get(get_team).patch(update_team).delete(delete_team),
        )
        // ---- roles ----
        .route("/api/admin/roles", get(list_roles).post(create_role))
        .route(
            "/api/admin/roles/:id",
            get(get_role).patch(update_role).delete(delete_role),
        )
        .route("/api/admin/roles/:id/permissions", post(grant_permission))
        .route(
            "/api/admin/roles/:id/permissions/:entity/:verb",
            delete(revoke_permission),
        )
        .route(
            "/api/admin/roles/:id/field-permissions",
            post(grant_field_permission),
        )
        .route(
            "/api/admin/roles/:id/field-permissions/:entity/:field",
            delete(revoke_field_permission),
        )
        // ---- org-wide defaults ----
        .route("/api/admin/owd", get(list_owd))
        .route("/api/admin/owd/:entity", get(get_owd).put(set_owd))
        // ---- users + role assignments ----
        .route("/api/admin/users", get(list_users).post(create_user))
        .route("/api/admin/users/:id", get(get_user).patch(update_user))
        .route(
            "/api/admin/users/:id/roles",
            get(list_user_roles).post(assign_role),
        )
        .route("/api/admin/users/:id/roles/:role_id", delete(revoke_role))
        .route("/api/admin/users/:id/password", post(reset_password))
}

// ===== gate =====

/// Every admin-security handler is superuser-only.
fn require_admin(user: &mda_security::Identity) -> ApiResult<()> {
    if user.is_superuser {
        Ok(())
    } else {
        Err(Error::Forbidden("admin security API requires an admin role".into()).into())
    }
}

/// Deserialize a present-but-null JSON value as `Some(None)`, distinguishing it
/// from an absent key (`None`). Powers the `Option<Option<T>>` PATCH fields
/// (`UpdateTeam::parent_id`, `UpdateUser::team_id`) so an explicit `null`
/// clears the column while an omitted key leaves it untouched.
fn deserialize_some<'de, T, D>(deserializer: D) -> Result<Option<T>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Deserialize::deserialize(deserializer).map(Some)
}

// ===== teams =====

#[derive(Debug, sqlx::FromRow, Serialize)]
struct TeamRow {
    id: Uuid,
    name: String,
    parent_id: Option<Uuid>,
    created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Deserialize)]
struct CreateTeam {
    name: String,
    #[serde(default)]
    parent_id: Option<Uuid>,
}

#[derive(Debug, Deserialize, Default)]
struct UpdateTeam {
    #[serde(default)]
    name: Option<String>,
    /// `Some(Some(id))` re-parents; `Some(None)` detaches (roots); `None`
    /// (absent) leaves it unchanged. serde's default collapses JSON `null` into
    /// `None`, so `deserialize_some` is required to keep the two distinct.
    #[serde(default, deserialize_with = "deserialize_some")]
    parent_id: Option<Option<Uuid>>,
}

/// `GET /api/admin/teams`
async fn list_teams(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
) -> ApiResult<Json<Vec<TeamRow>>> {
    require_admin(&user)?;
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    let rows: Vec<TeamRow> =
        sqlx::query_as("SELECT id, name, parent_id, created_at FROM sec.sec_team ORDER BY name")
            .fetch_all(&mut *tx)
            .await
            .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(Json(rows))
}

/// `POST /api/admin/teams`
async fn create_team(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Json(body): Json<CreateTeam>,
) -> ApiResult<(StatusCode, Json<TeamRow>)> {
    require_admin(&user)?;
    validate_team_name(&body.name)?;
    if let Some(parent) = body.parent_id {
        ensure_team_in_tenant(&st.pool, user.tenant_id, parent).await?;
    }
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    let row: TeamRow = sqlx::query_as(
        "INSERT INTO sec.sec_team (tenant_id, name, parent_id)
         VALUES ($1, $2, $3)
         RETURNING id, name, parent_id, created_at",
    )
    .bind(user.tenant_id)
    .bind(&body.name)
    .bind(body.parent_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| match e {
        sqlx::Error::Database(ref d) if d.is_unique_violation() => {
            Error::Conflict("team name already exists".into())
        }
        other => Error::internal(other),
    })?;
    tx.commit().await.map_err(Error::internal)?;
    Ok((StatusCode::CREATED, Json(row)))
}

/// `GET /api/admin/teams/:id`
async fn get_team(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<TeamRow>> {
    require_admin(&user)?;
    let row = fetch_team(&st.pool, user.tenant_id, id).await?;
    Ok(Json(row))
}

/// `PATCH /api/admin/teams/:id` — rename and/or re-parent. `parent_id: null`
/// detaches (roots) the team. A self-/cycle-creating parent is rejected.
async fn update_team(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateTeam>,
) -> ApiResult<Json<TeamRow>> {
    require_admin(&user)?;
    if let Some(ref name) = body.name {
        validate_team_name(name)?;
    }
    if let Some(Some(parent)) = body.parent_id {
        // reject a self-loop or a parent that would create a cycle (the proposed
        // parent's ancestor chain must not pass through this team).
        if parent == id {
            return Err(Error::Invalid("a team cannot be its own parent".into()).into());
        }
        ensure_team_in_tenant(&st.pool, user.tenant_id, parent).await?;
        if would_cycle(&st.pool, user.tenant_id, id, parent).await? {
            return Err(Error::Invalid("parent_id would create a cycle".into()).into());
        }
    }
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    // Fetch current under the tenant GUC so a wrong-tenant id is a 404.
    let existing: TeamRow =
        sqlx::query_as("SELECT id, name, parent_id, created_at FROM sec.sec_team WHERE id = $1")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(Error::internal)?
            .ok_or_else(|| Error::NotFound(format!("team {id}")))?;
    let name = body.name.unwrap_or(existing.name);
    let parent_id = match body.parent_id {
        Some(opt) => opt,
        None => existing.parent_id,
    };
    let row: TeamRow = sqlx::query_as(
        "UPDATE sec.sec_team SET name = $2, parent_id = $3 WHERE id = $1
         RETURNING id, name, parent_id, created_at",
    )
    .bind(id)
    .bind(name)
    .bind(parent_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| match e {
        sqlx::Error::Database(ref d) if d.is_unique_violation() => {
            Error::Conflict("team name already exists".into())
        }
        other => Error::internal(other),
    })?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(Json(row))
}

/// `DELETE /api/admin/teams/:id` — children are re-rooted (parent_id cleared);
/// members' `team_id` is cleared (ON DELETE SET NULL semantics on sec_user).
async fn delete_team(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    require_admin(&user)?;
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    let n = sqlx::query("DELETE FROM sec.sec_team WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(Error::internal)?
        .rows_affected();
    tx.commit().await.map_err(Error::internal)?;
    if n == 0 {
        Err(Error::NotFound(format!("team {id}")).into())
    } else {
        Ok(StatusCode::NO_CONTENT)
    }
}

async fn fetch_team(pool: &sqlx::PgPool, tenant: Uuid, id: Uuid) -> ApiResult<TeamRow> {
    let mut tx = pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, tenant).await?;
    let row: TeamRow =
        sqlx::query_as("SELECT id, name, parent_id, created_at FROM sec.sec_team WHERE id = $1")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(Error::internal)?
            .ok_or_else(|| Error::NotFound(format!("team {id}")))?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(row)
}

/// True if making `parent` the parent of `id` would create a cycle — i.e. `id`
/// is already an ancestor of `parent` (the proposed parent's upward chain
/// reaches `id`).
async fn would_cycle(pool: &sqlx::PgPool, tenant: Uuid, id: Uuid, parent: Uuid) -> ApiResult<bool> {
    let mut tx = pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, tenant).await?;
    let (hits,): (i64,) = sqlx::query_as(
        "WITH RECURSIVE up(tid) AS (
                SELECT $2
                UNION ALL
                SELECT t.parent_id FROM sec.sec_team t JOIN up ON t.id = up.tid
                WHERE t.parent_id IS NOT NULL)
         SELECT count(*) FROM up WHERE tid = $1",
    )
    .bind(id)
    .bind(parent)
    .fetch_one(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(hits > 0)
}

fn validate_team_name(name: &str) -> ApiResult<()> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(Error::Invalid("team name must not be empty".into()).into());
    }
    Ok(())
}

/// Confirm a team id exists in this tenant (404 otherwise). Guards against
/// cross-tenant parent references and dangling ids.
async fn ensure_team_in_tenant(pool: &sqlx::PgPool, tenant: Uuid, id: Uuid) -> ApiResult<()> {
    fetch_team(pool, tenant, id).await.map(|_| ())
}

// ===== roles =====

#[derive(Debug, sqlx::FromRow, Serialize)]
struct RoleRow {
    id: Uuid,
    name: String,
    created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Deserialize)]
struct CreateRole {
    name: String,
}

#[derive(Debug, Deserialize)]
struct UpdateRole {
    name: String,
}

/// `GET /api/admin/roles` — roles with their (entity, verb) permission set and
/// field-permission set, plus the assigned user count.
async fn list_roles(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
) -> ApiResult<Json<Vec<serde_json::Value>>> {
    require_admin(&user)?;
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    let roles: Vec<RoleRow> =
        sqlx::query_as("SELECT id, name, created_at FROM sec.sec_role ORDER BY name")
            .fetch_all(&mut *tx)
            .await
            .map_err(Error::internal)?;
    let mut out = Vec::with_capacity(roles.len());
    for r in &roles {
        let perms: Vec<(String, String)> = sqlx::query_as(
            "SELECT entity, verb FROM sec.sec_permission WHERE role_id = $1 ORDER BY entity, verb",
        )
        .bind(r.id)
        .fetch_all(&mut *tx)
        .await
        .map_err(Error::internal)?;
        let fps: Vec<(String, String, String)> =
            sqlx::query_as("SELECT entity, field, access FROM sec.sec_field_permission WHERE role_id = $1 ORDER BY entity, field")
                .bind(r.id)
                .fetch_all(&mut *tx)
                .await
                .map_err(Error::internal)?;
        let (users,): (i64,) =
            sqlx::query_as("SELECT count(*) FROM sec.sec_role_assignment WHERE role_id = $1")
                .bind(r.id)
                .fetch_one(&mut *tx)
                .await
                .map_err(Error::internal)?;
        out.push(serde_json::json!({
            "id": r.id,
            "name": r.name,
            "created_at": r.created_at,
            "permissions": perms.into_iter().map(|(e, v)| serde_json::json!({"entity": e, "verb": v})).collect::<Vec<_>>(),
            "field_permissions": fps.into_iter().map(|(e, f, a)| serde_json::json!({"entity": e, "field": f, "access": a})).collect::<Vec<_>>(),
            "user_count": users,
        }));
    }
    tx.commit().await.map_err(Error::internal)?;
    Ok(Json(out))
}

/// `POST /api/admin/roles`
async fn create_role(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Json(body): Json<CreateRole>,
) -> ApiResult<(StatusCode, Json<RoleRow>)> {
    require_admin(&user)?;
    if body.name.trim().is_empty() {
        return Err(Error::Invalid("role name must not be empty".into()).into());
    }
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    let row: RoleRow = sqlx::query_as(
        "INSERT INTO sec.sec_role (tenant_id, name)
         VALUES ($1, $2)
         RETURNING id, name, created_at",
    )
    .bind(user.tenant_id)
    .bind(&body.name)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| match e {
        sqlx::Error::Database(ref d) if d.is_unique_violation() => {
            Error::Conflict("role name already exists".into())
        }
        other => Error::internal(other),
    })?;
    tx.commit().await.map_err(Error::internal)?;
    Ok((StatusCode::CREATED, Json(row)))
}

/// `GET /api/admin/roles/:id`
async fn get_role(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<RoleRow>> {
    require_admin(&user)?;
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    let row: RoleRow =
        sqlx::query_as("SELECT id, name, created_at FROM sec.sec_role WHERE id = $1")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(Error::internal)?
            .ok_or_else(|| Error::NotFound(format!("role {id}")))?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(Json(row))
}

/// `PATCH /api/admin/roles/:id`
async fn update_role(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateRole>,
) -> ApiResult<Json<RoleRow>> {
    require_admin(&user)?;
    if body.name.trim().is_empty() {
        return Err(Error::Invalid("role name must not be empty".into()).into());
    }
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    let row: RoleRow = sqlx::query_as(
        "UPDATE sec.sec_role SET name = $2 WHERE id = $1
         RETURNING id, name, created_at",
    )
    .bind(id)
    .bind(&body.name)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| match e {
        sqlx::Error::Database(ref d) if d.is_unique_violation() => {
            Error::Conflict("role name already exists".into())
        }
        other => Error::internal(other),
    })?
    .ok_or_else(|| Error::NotFound(format!("role {id}")))?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(Json(row))
}

/// `DELETE /api/admin/roles/:id` — cascades to permissions / assignments.
async fn delete_role(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    require_admin(&user)?;
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    let n = sqlx::query("DELETE FROM sec.sec_role WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(Error::internal)?
        .rows_affected();
    tx.commit().await.map_err(Error::internal)?;
    if n == 0 {
        Err(Error::NotFound(format!("role {id}")).into())
    } else {
        Ok(StatusCode::NO_CONTENT)
    }
}

// ===== permissions =====

#[derive(Debug, Deserialize)]
struct PermissionBody {
    entity: String,
    verb: String,
}

/// `POST /api/admin/roles/:id/permissions` — grant (entity, verb). `entity` or
/// `verb` may be `*`.
async fn grant_permission(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<PermissionBody>,
) -> ApiResult<StatusCode> {
    require_admin(&user)?;
    if body.entity.trim().is_empty() || body.verb.trim().is_empty() {
        return Err(Error::Invalid("entity and verb must not be empty".into()).into());
    }
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    ensure_role_visible(&mut tx, id).await?;
    sqlx::query(
        "INSERT INTO sec.sec_permission (role_id, entity, verb) VALUES ($1, $2, $3)
         ON CONFLICT DO NOTHING",
    )
    .bind(id)
    .bind(&body.entity)
    .bind(&body.verb)
    .execute(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/admin/roles/:id/permissions/:entity/:verb`
async fn revoke_permission(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path((id, entity, verb)): Path<(Uuid, String, String)>,
) -> ApiResult<StatusCode> {
    require_admin(&user)?;
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    sqlx::query("DELETE FROM sec.sec_permission WHERE role_id = $1 AND entity = $2 AND verb = $3")
        .bind(id)
        .bind(entity)
        .bind(verb)
        .execute(&mut *tx)
        .await
        .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
struct FieldPermissionBody {
    entity: String,
    field: String,
    access: String,
}

/// `POST /api/admin/roles/:id/field-permissions` — set field access
/// (none | read | write).
async fn grant_field_permission(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<FieldPermissionBody>,
) -> ApiResult<StatusCode> {
    require_admin(&user)?;
    if !matches!(body.access.as_str(), "none" | "read" | "write") {
        return Err(Error::Invalid("access must be one of: none, read, write".into()).into());
    }
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    ensure_role_visible(&mut tx, id).await?;
    sqlx::query(
        "INSERT INTO sec.sec_field_permission (role_id, entity, field, access)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (role_id, entity, field) DO UPDATE SET access = EXCLUDED.access",
    )
    .bind(id)
    .bind(&body.entity)
    .bind(&body.field)
    .bind(&body.access)
    .execute(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/admin/roles/:id/field-permissions/:entity/:field`
async fn revoke_field_permission(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path((id, entity, field)): Path<(Uuid, String, String)>,
) -> ApiResult<StatusCode> {
    require_admin(&user)?;
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    sqlx::query(
        "DELETE FROM sec.sec_field_permission WHERE role_id = $1 AND entity = $2 AND field = $3",
    )
    .bind(id)
    .bind(entity)
    .bind(field)
    .execute(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(StatusCode::NO_CONTENT)
}

// ===== org-wide defaults =====

#[derive(Debug, sqlx::FromRow, Serialize)]
struct OwdRow {
    entity: String,
    default_access: String,
}

/// `GET /api/admin/owd`
async fn list_owd(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
) -> ApiResult<Json<Vec<OwdRow>>> {
    require_admin(&user)?;
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    let rows: Vec<OwdRow> =
        sqlx::query_as("SELECT entity, default_access FROM sec.sec_owd ORDER BY entity")
            .fetch_all(&mut *tx)
            .await
            .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(Json(rows))
}

/// `GET /api/admin/owd/:entity`
async fn get_owd(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(entity): Path<String>,
) -> ApiResult<Json<OwdRow>> {
    require_admin(&user)?;
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    let row: OwdRow =
        sqlx::query_as("SELECT entity, default_access FROM sec.sec_owd WHERE entity = $1")
            .bind(&entity)
            .fetch_optional(&mut *tx)
            .await
            .map_err(Error::internal)?
            .ok_or_else(|| Error::NotFound(format!("no OWD for {entity}")))?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(Json(row))
}

#[derive(Debug, Deserialize)]
struct SetOwd {
    default_access: String,
}

/// `PUT /api/admin/owd/:entity` — set the org-wide default
/// (private | team | public_read | public_read_write).
async fn set_owd(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(entity): Path<String>,
    Json(body): Json<SetOwd>,
) -> ApiResult<Json<OwdRow>> {
    require_admin(&user)?;
    if !matches!(
        body.default_access.as_str(),
        "private" | "team" | "public_read" | "public_read_write"
    ) {
        return Err(Error::Invalid(
            "default_access must be one of: private, team, public_read, public_read_write".into(),
        )
        .into());
    }
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    let row: OwdRow = sqlx::query_as(
        "INSERT INTO sec.sec_owd (tenant_id, entity, default_access)
         VALUES ($1, $2, $3)
         ON CONFLICT (tenant_id, entity) DO UPDATE SET default_access = EXCLUDED.default_access
         RETURNING entity, default_access",
    )
    .bind(user.tenant_id)
    .bind(&entity)
    .bind(&body.default_access)
    .fetch_one(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(Json(row))
}

// ===== users + assignments =====

#[derive(Debug, sqlx::FromRow, Serialize)]
struct UserRow {
    id: Uuid,
    email: String,
    name: Option<String>,
    team_id: Option<Uuid>,
    active: bool,
    created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Deserialize)]
struct CreateUser {
    email: String,
    name: Option<String>,
    password: String,
    #[serde(default)]
    team_id: Option<Uuid>,
    #[serde(default = "default_true")]
    active: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, Default)]
struct UpdateUser {
    #[serde(default)]
    name: Option<String>,
    /// `Some(Some(id))` sets the team; `Some(None)` clears it; `None` (absent)
    /// leaves it unchanged (see [`UpdateTeam::parent_id`]).
    #[serde(default, deserialize_with = "deserialize_some")]
    team_id: Option<Option<Uuid>>,
    #[serde(default)]
    active: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct ResetPassword {
    password: String,
}

#[derive(Debug, Deserialize)]
struct AssignRole {
    role_id: Uuid,
}

/// `GET /api/admin/users` — list users (never password hashes).
async fn list_users(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
) -> ApiResult<Json<Vec<UserRow>>> {
    require_admin(&user)?;
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    let rows: Vec<UserRow> = sqlx::query_as(
        "SELECT id, email, name, team_id, active, created_at FROM sec.sec_user ORDER BY email",
    )
    .fetch_all(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(Json(rows))
}

/// `POST /api/admin/users` — create a user (password hashed server-side).
async fn create_user(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Json(body): Json<CreateUser>,
) -> ApiResult<(StatusCode, Json<UserRow>)> {
    require_admin(&user)?;
    if body.email.trim().is_empty() || body.password.is_empty() {
        return Err(Error::Invalid("email and password are required".into()).into());
    }
    if let Some(team) = body.team_id {
        ensure_team_in_tenant(&st.pool, user.tenant_id, team).await?;
    }
    let hash =
        hash_password(&body.password).map_err(|e| Error::Internal(anyhow::anyhow!("{e}")))?;
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    let row: UserRow = sqlx::query_as(
        "INSERT INTO sec.sec_user (tenant_id, email, name, password_hash, team_id, active)
         VALUES ($1, $2, $3, $4, $5, $6)
         RETURNING id, email, name, team_id, active, created_at",
    )
    .bind(user.tenant_id)
    .bind(&body.email)
    .bind(&body.name)
    .bind(&hash)
    .bind(body.team_id)
    .bind(body.active)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| match e {
        sqlx::Error::Database(ref d) if d.is_unique_violation() => {
            Error::Conflict("email already exists".into())
        }
        other => Error::internal(other),
    })?;
    tx.commit().await.map_err(Error::internal)?;
    Ok((StatusCode::CREATED, Json(row)))
}

/// `GET /api/admin/users/:id`
async fn get_user(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<UserRow>> {
    require_admin(&user)?;
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    let row: UserRow = sqlx::query_as(
        "SELECT id, email, name, team_id, active, created_at FROM sec.sec_user WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(Error::internal)?
    .ok_or_else(|| Error::NotFound(format!("user {id}")))?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(Json(row))
}

/// `PATCH /api/admin/users/:id` — update name / team / active. `team_id: null`
/// clears the team.
async fn update_user(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateUser>,
) -> ApiResult<Json<UserRow>> {
    require_admin(&user)?;
    if let Some(Some(team)) = body.team_id {
        ensure_team_in_tenant(&st.pool, user.tenant_id, team).await?;
    }
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    let existing: UserRow = sqlx::query_as(
        "SELECT id, email, name, team_id, active, created_at FROM sec.sec_user WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(Error::internal)?
    .ok_or_else(|| Error::NotFound(format!("user {id}")))?;
    let name = body.name.or(existing.name);
    let team_id = match body.team_id {
        Some(opt) => opt,
        None => existing.team_id,
    };
    let active = body.active.unwrap_or(existing.active);
    let row: UserRow = sqlx::query_as(
        "UPDATE sec.sec_user SET name = $2, team_id = $3, active = $4, updated_at = now()
         WHERE id = $1
         RETURNING id, email, name, team_id, active, created_at",
    )
    .bind(id)
    .bind(name)
    .bind(team_id)
    .bind(active)
    .fetch_one(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(Json(row))
}

/// `POST /api/admin/users/:id/password` — admin password reset.
async fn reset_password(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<ResetPassword>,
) -> ApiResult<StatusCode> {
    require_admin(&user)?;
    if body.password.is_empty() {
        return Err(Error::Invalid("password must not be empty".into()).into());
    }
    let hash =
        hash_password(&body.password).map_err(|e| Error::Internal(anyhow::anyhow!("{e}")))?;
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    let n =
        sqlx::query("UPDATE sec.sec_user SET password_hash = $2, updated_at = now() WHERE id = $1")
            .bind(id)
            .bind(&hash)
            .execute(&mut *tx)
            .await
            .map_err(Error::internal)?
            .rows_affected();
    tx.commit().await.map_err(Error::internal)?;
    if n == 0 {
        Err(Error::NotFound(format!("user {id}")).into())
    } else {
        Ok(StatusCode::NO_CONTENT)
    }
}

/// `GET /api/admin/users/:id/roles` — the roles assigned to a user.
async fn list_user_roles(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Vec<RoleRow>>> {
    require_admin(&user)?;
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    let rows: Vec<RoleRow> = sqlx::query_as(
        "SELECT r.id, r.name, r.created_at
           FROM sec.sec_role r JOIN sec.sec_role_assignment a ON a.role_id = r.id
          WHERE a.user_id = $1 ORDER BY r.name",
    )
    .bind(id)
    .fetch_all(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(Json(rows))
}

/// `POST /api/admin/users/:id/roles` — assign a role to a user.
async fn assign_role(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<AssignRole>,
) -> ApiResult<StatusCode> {
    require_admin(&user)?;
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    ensure_role_visible(&mut tx, body.role_id).await?;
    // 404 if the user doesn't exist in this tenant (rather than a silent insert).
    let (exists,): (bool,) =
        sqlx::query_as("SELECT EXISTS(SELECT 1 FROM sec.sec_user WHERE id = $1)")
            .bind(id)
            .fetch_one(&mut *tx)
            .await
            .map_err(Error::internal)?;
    if !exists {
        return Err(Error::NotFound(format!("user {id}")).into());
    }
    sqlx::query("INSERT INTO sec.sec_role_assignment (user_id, role_id) VALUES ($1, $2) ON CONFLICT DO NOTHING")
        .bind(id)
        .bind(body.role_id)
        .execute(&mut *tx)
        .await
        .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/admin/users/:id/roles/:role_id`
async fn revoke_role(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path((id, role_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    require_admin(&user)?;
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    sqlx::query("DELETE FROM sec.sec_role_assignment WHERE user_id = $1 AND role_id = $2")
        .bind(id)
        .bind(role_id)
        .execute(&mut *tx)
        .await
        .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(StatusCode::NO_CONTENT)
}

/// 404 if the role id is not visible under the current tenant GUC.
async fn ensure_role_visible(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: Uuid,
) -> ApiResult<()> {
    let (exists,): (bool,) =
        sqlx::query_as("SELECT EXISTS(SELECT 1 FROM sec.sec_role WHERE id = $1)")
            .bind(id)
            .fetch_one(&mut **tx)
            .await
            .map_err(Error::internal)?;
    if exists {
        Ok(())
    } else {
        Err(Error::NotFound(format!("role {id}")).into())
    }
}
