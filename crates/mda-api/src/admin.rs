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

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use mda_core::Error;
use mda_security::{hash_password, set_tenant};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
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
        // ---- sharing rules (ADR-0013) ----
        .route(
            "/api/admin/share-rules",
            get(list_share_rules).post(create_share_rule),
        )
        .route(
            "/api/admin/share-rules/:id",
            axum::routing::patch(update_share_rule).delete(delete_share_rule),
        )
        .route(
            "/api/admin/share-rules/:id/recompute",
            post(recompute_share_rule),
        )
        // ---- role hierarchy ----
        .route("/api/admin/roles/:id/parents", get(list_role_parents))
        .route(
            "/api/admin/roles/:id/parents/:parent_id",
            post(add_role_parent).delete(remove_role_parent),
        )
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

// ===== sharing rules (ADR-0013) =====
//
// A criteria-based sharing rule materializes "records matching <condition> are
// visible to <principal>" into sec_record_share. Per-record recompute is
// synchronous in the write path (mda-data::sharing); this surface manages the
// rules themselves:
//   - CREATE: insert + bounded materialization, NO epoch bump (purely additive
//     grants can never revoke — ADR-0013 rule 3);
//   - PATCH (condition/access/principal/active): epoch bump (instant revoke of
//     everything materialized under the old epoch) + re-materialization;
//   - DELETE: rule row gone → its shares cascade away instantly;
//   - POST /recompute: resumable keyset-batched catch-up (from=<last scanned id>)
//     for rules whose automatic pass hit the bound.

#[derive(Debug, sqlx::FromRow, Serialize)]
struct ShareRuleRow {
    id: Uuid,
    entity: String,
    condition: serde_json::Value,
    principal_id: Uuid,
    access: String,
    epoch: i64,
    active: bool,
    created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Deserialize)]
struct CreateShareRule {
    entity: String,
    /// Bounded-DSL condition evaluated against the record (§5.2).
    condition: serde_json::Value,
    /// A user id or a team id in this tenant.
    principal_id: Uuid,
    /// read | write.
    access: String,
}

#[derive(Deserialize)]
struct UpdateShareRule {
    condition: Option<serde_json::Value>,
    principal_id: Option<Uuid>,
    access: Option<String>,
    active: Option<bool>,
}

#[derive(Deserialize)]
struct RecomputeQuery {
    from: Option<Uuid>,
    #[serde(default = "default_recompute_limit")]
    limit: i64,
}
fn default_recompute_limit() -> i64 {
    5000
}
const MAX_RECOMPUTE_LIMIT: i64 = 50_000;
/// Keyset batch size for the recompute scan.
const RECOMPUTE_BATCH: i64 = 500;

async fn list_share_rules(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
) -> ApiResult<Json<Vec<ShareRuleRow>>> {
    require_admin(&user)?;
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    let rows: Vec<ShareRuleRow> = sqlx::query_as(
        "SELECT id, entity, condition, principal_id, access, epoch, active, created_at \
         FROM sec.sec_share_rule ORDER BY entity, created_at, id",
    )
    .fetch_all(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(Json(rows))
}

async fn create_share_rule(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Json(body): Json<CreateShareRule>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    require_admin(&user)?;
    validate_share_rule_body(&st, &user, &body.entity, body.principal_id, &body.access).await?;
    parse_condition(&body.condition)?;
    parse_condition(&body.condition)?;
    let def = crate::data::entity_def(&st, user.tenant_id, &body.entity).await?;
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    let row: ShareRuleRow = sqlx::query_as(
        "INSERT INTO sec.sec_share_rule (tenant_id, entity, condition, principal_id, access) \
         VALUES ($1, $2, $3, $4, $5) \
         RETURNING id, entity, condition, principal_id, access, epoch, active, created_at",
    )
    .bind(user.tenant_id)
    .bind(&body.entity)
    .bind(&body.condition)
    .bind(body.principal_id)
    .bind(&body.access)
    .fetch_one(&mut *tx)
    .await
    .map_err(Error::internal)?;
    // Additive grant: no epoch bump; materialize what we can within the bound.
    let stats = materialize_rule(
        &mut tx,
        user.tenant_id,
        &def,
        &row,
        None,
        default_recompute_limit(),
    )
    .await?;
    tx.commit().await.map_err(Error::internal)?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({"rule": row, "recompute": stats})),
    ))
}

async fn update_share_rule(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateShareRule>,
) -> ApiResult<Json<Value>> {
    require_admin(&user)?;
    if let Some(ref access) = body.access {
        validate_access(access)?;
    }
    if let Some(principal) = body.principal_id {
        ensure_principal_in_tenant(&st.pool, user.tenant_id, principal).await?;
    }
    if let Some(ref cond) = body.condition {
        parse_condition(cond)?;
    }
    // the rule's entity (fixed at create) decides which table to materialize
    let entity: String = {
        let mut tx = st.pool.begin().await.map_err(Error::internal)?;
        set_tenant(&mut tx, user.tenant_id).await?;
        let e: Option<(String,)> = sqlx::query_as(
            "SELECT entity FROM sec.sec_share_rule WHERE tenant_id = $1 AND id = $2",
        )
        .bind(user.tenant_id)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(Error::internal)?;
        tx.commit().await.map_err(Error::internal)?;
        e.ok_or_else(|| Error::NotFound(format!("share rule {id}")))?
            .0
    };
    let def = crate::data::entity_def(&st, user.tenant_id, &entity).await?;
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    let existing: ShareRuleRow = sqlx::query_as(
        "SELECT id, entity, condition, principal_id, access, epoch, active, created_at \
         FROM sec.sec_share_rule WHERE tenant_id = $1 AND id = $2",
    )
    .bind(user.tenant_id)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(Error::internal)?
    .ok_or_else(|| Error::NotFound(format!("share rule {id}")))?;

    let changed = body.condition.is_some()
        || body.principal_id.is_some()
        || body.access.is_some()
        || body.active.is_some();
    let row: ShareRuleRow = sqlx::query_as(
        "UPDATE sec.sec_share_rule SET condition = $3, principal_id = $4, access = $5, active = $6 \
         WHERE tenant_id = $1 AND id = $2 \
         RETURNING id, entity, condition, principal_id, access, epoch, active, created_at",
    )
    .bind(user.tenant_id)
    .bind(id)
    .bind(body.condition.clone().unwrap_or(existing.condition.clone()))
    .bind(body.principal_id.unwrap_or(existing.principal_id))
    .bind(body.access.clone().unwrap_or(existing.access.clone()))
    .bind(body.active.unwrap_or(existing.active))
    .fetch_one(&mut *tx)
    .await
    .map_err(Error::internal)?;

    let stats;
    if !changed {
        stats = serde_json::json!({"scanned": 0, "materialized": 0, "truncated": false});
    } else if !row.active {
        // Deactivation is a pure narrowing: bump the epoch (instant revoke) and
        // drop everything this rule materialized. Nothing to re-add.
        mda_data::bump_epoch(&mut tx, user.tenant_id, id).await?;
        sqlx::query("DELETE FROM sec.sec_record_share WHERE tenant_id = $1 AND rule_id = $2")
            .bind(user.tenant_id)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(Error::internal)?;
        stats = serde_json::json!({"scanned": 0, "materialized": 0, "truncated": false});
    } else {
        // Edit: revoke-safe — bump the epoch, drop the old rows, re-materialize
        // under the new epoch (bounded; /recompute continues if truncated).
        mda_data::bump_epoch(&mut tx, user.tenant_id, id).await?;
        sqlx::query("DELETE FROM sec.sec_record_share WHERE tenant_id = $1 AND rule_id = $2")
            .bind(user.tenant_id)
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(Error::internal)?;
        stats = materialize_rule(
            &mut tx,
            user.tenant_id,
            &def,
            &row,
            None,
            default_recompute_limit(),
        )
        .await?;
    }
    tx.commit().await.map_err(Error::internal)?;
    Ok(Json(serde_json::json!({"rule": row, "recompute": stats})))
}

async fn delete_share_rule(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    require_admin(&user)?;
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    let res = sqlx::query("DELETE FROM sec.sec_share_rule WHERE tenant_id = $1 AND id = $2")
        .bind(user.tenant_id)
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    if res.rows_affected() == 0 {
        return Err(Error::NotFound(format!("share rule {id}")).into());
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/admin/share-rules/:id/recompute?from=<id>&limit=<n>` — resumable
/// grant-side catch-up. Revocation never needs this (the epoch gate is
/// authoritative); this only re-materializes current matches, in keyset order,
/// reporting the last scanned id so a truncated pass can resume.
async fn recompute_share_rule(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
    Query(q): Query<RecomputeQuery>,
) -> ApiResult<Json<Value>> {
    require_admin(&user)?;
    let limit = q.limit.clamp(1, MAX_RECOMPUTE_LIMIT);
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    let row: ShareRuleRow = sqlx::query_as(
        "SELECT id, entity, condition, principal_id, access, epoch, active, created_at \
         FROM sec.sec_share_rule WHERE tenant_id = $1 AND id = $2",
    )
    .bind(user.tenant_id)
    .bind(id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(Error::internal)?
    .ok_or_else(|| Error::NotFound(format!("share rule {id}")))?;
    if !row.active {
        return Err(Error::Invalid("cannot recompute an inactive rule".into()).into());
    }
    tx.commit().await.map_err(Error::internal)?;
    // resolve the entity definition (metadata cache) with the tx closed, then
    // scan under a fresh transaction
    let def = crate::data::entity_def(&st, user.tenant_id, &row.entity).await?;
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    let stats = materialize_rule(&mut tx, user.tenant_id, &def, &row, q.from, limit).await?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(Json(stats))
}

/// Scan the rule's entity table in keyset batches, evaluating the condition in
/// the bounded DSL and upserting matching shares at the rule's current epoch.
/// Additive-only (`ON CONFLICT DO NOTHING`) so a manual share's access level is
/// never downgraded by a rule.
async fn materialize_rule(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: Uuid,
    def: &mda_meta::EntityDefinition,
    rule: &ShareRuleRow,
    from: Option<Uuid>,
    limit: i64,
) -> ApiResult<serde_json::Value> {
    let table = def.entity.table_name.clone();
    let reg = mda_expression::Registry::new();
    let expr: mda_expression::Expr = serde_json::from_value(rule.condition.clone())
        .map_err(|e| Error::Invalid(format!("bad condition: {e}")))?;

    let mut cursor = from;
    let mut scanned: i64 = 0;
    let mut materialized: i64 = 0;
    let mut truncated = false;
    let mut last_id: Option<Uuid> = None;
    'outer: loop {
        let rows: Vec<(Uuid, serde_json::Value)> = sqlx::query_as(&format!(
            "SELECT t.id, to_jsonb(t.*) FROM biz.{table} t WHERE t.tenant_id = $1 AND ($2::uuid IS NULL OR t.id > $2) ORDER BY t.id LIMIT {RECOMPUTE_BATCH}"
        ))
        .bind(tenant)
        .bind(cursor)
        .fetch_all(&mut **tx)
        .await
        .map_err(Error::internal)?;
        if rows.is_empty() {
            break;
        }
        for (rid, doc) in rows {
            scanned += 1;
            cursor = Some(rid);
            last_id = Some(rid);
            let record = mda_data::reconstruct(def, doc);
            let matches = mda_expression::eval(&expr, &record, &reg)
                .map(|v| mda_expression::truth(&v))
                .unwrap_or(false);
            if matches {
                sqlx::query(
                    "INSERT INTO sec.sec_record_share \
                         (tenant_id, entity, record_id, principal_id, access, rule_id, epoch) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7) \
                     ON CONFLICT (tenant_id, record_id, principal_id) DO NOTHING",
                )
                .bind(tenant)
                .bind(&rule.entity)
                .bind(rid)
                .bind(rule.principal_id)
                .bind(&rule.access)
                .bind(rule.id)
                .bind(rule.epoch)
                .execute(&mut **tx)
                .await
                .map_err(Error::internal)?;
                materialized += 1;
            }
            if scanned >= limit {
                truncated = true;
                break 'outer;
            }
        }
    }
    Ok(serde_json::json!({
        "scanned": scanned,
        "materialized": materialized,
        "truncated": truncated,
        "last_id": last_id,
    }))
}

fn validate_access(access: &str) -> ApiResult<()> {
    match access {
        "read" | "write" => Ok(()),
        other => Err(Error::Invalid(format!("access must be read|write, got {other}")).into()),
    }
}

async fn ensure_principal_in_tenant(
    pool: &sqlx::PgPool,
    tenant: Uuid,
    principal: Uuid,
) -> ApiResult<()> {
    let mut tx = pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, tenant).await?;
    let user: Option<(i32,)> = sqlx::query_as("SELECT 1 FROM sec.sec_user WHERE id = $1")
        .bind(principal)
        .fetch_optional(&mut *tx)
        .await
        .map_err(Error::internal)?;
    let team: Option<(i32,)> = sqlx::query_as("SELECT 1 FROM sec.sec_team WHERE id = $1")
        .bind(principal)
        .fetch_optional(&mut *tx)
        .await
        .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    if user.is_some() || team.is_some() {
        Ok(())
    } else {
        Err(Error::Invalid(format!(
            "principal {principal} is not a user or team in this tenant"
        ))
        .into())
    }
}

async fn validate_share_rule_body(
    st: &AppState,
    user: &mda_security::Identity,
    entity: &str,
    principal: Uuid,
    access: &str,
) -> ApiResult<()> {
    validate_access(access)?;
    ensure_principal_in_tenant(&st.pool, user.tenant_id, principal).await?;
    // the entity must exist (shares are keyed by API entity name)
    mda_meta::loader::entity_id_by_name(&st.pool, user.tenant_id, entity).await?;
    Ok(())
}

// ===== role hierarchy (ADR-0013/ADR-0026) =====
//
// `sec_role_hierarchy(role_id, parent_id)` parents one role under another; a
// user holding a parent role READS records owned by users in descendant roles
// ("see records below me"). Evaluation is LIVE (recursive CTE in the record
// read predicate, mirroring ADR-0025's team hierarchy): a re-parent or role
// removal is effective on the next query — no materialization, no epoch, no
// revocation lag at all. Writes are never amplified by hierarchy.

/// `GET /api/admin/roles/:id/parents`
async fn list_role_parents(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    require_admin(&user)?;
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    let rows: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT r.id, r.name FROM sec.sec_role_hierarchy h \
         JOIN sec.sec_role r ON r.id = h.parent_id \
         WHERE h.tenant_id = $1 AND h.role_id = $2 ORDER BY r.name",
    )
    .bind(user.tenant_id)
    .bind(id)
    .fetch_all(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(Json(serde_json::json!({"role_id": id, "parents": rows})))
}

/// `POST /api/admin/roles/:id/parents/:parent_id` — parent a role (a role may
/// have several parents; visibility unions them). Self-parenting and cycles are
/// rejected (the graph must stay a DAG).
async fn add_role_parent(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path((id, parent_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    require_admin(&user)?;
    if id == parent_id {
        return Err(Error::Invalid("a role cannot be its own parent".into()).into());
    }
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    for role in [id, parent_id] {
        let exists: Option<(i32,)> = sqlx::query_as("SELECT 1 FROM sec.sec_role WHERE id = $1")
            .bind(role)
            .fetch_optional(&mut *tx)
            .await
            .map_err(Error::internal)?;
        if exists.is_none() {
            return Err(Error::NotFound(format!("role {role}")).into());
        }
    }
    // cycle check: walking UP from parent_id must never reach `id`
    let hits: Option<(i32,)> = sqlx::query_as(
        "WITH RECURSIVE up(rid) AS ( \
            SELECT $2::uuid \
            UNION ALL \
            SELECT h.parent_id FROM sec.sec_role_hierarchy h JOIN up ON h.role_id = up.rid) \
         SELECT 1 WHERE $1::uuid IN (SELECT rid FROM up)",
    )
    .bind(id)
    .bind(parent_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(Error::internal)?;
    if hits.is_some() {
        return Err(Error::Invalid("cycle: that role is already an ancestor".into()).into());
    }
    sqlx::query(
        "INSERT INTO sec.sec_role_hierarchy (tenant_id, role_id, parent_id) VALUES ($1, $2, $3) \
         ON CONFLICT DO NOTHING",
    )
    .bind(user.tenant_id)
    .bind(id)
    .bind(parent_id)
    .execute(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(StatusCode::CREATED)
}

/// `DELETE /api/admin/roles/:id/parents/:parent_id` — detach.
async fn remove_role_parent(
    State(st): State<AppState>,
    AuthUser(user): AuthUser,
    Path((id, parent_id)): Path<(Uuid, Uuid)>,
) -> ApiResult<StatusCode> {
    require_admin(&user)?;
    let mut tx = st.pool.begin().await.map_err(Error::internal)?;
    set_tenant(&mut tx, user.tenant_id).await?;
    let res =
        sqlx::query("DELETE FROM sec.sec_role_hierarchy WHERE tenant_id = $1 AND role_id = $2 AND parent_id = $3")
            .bind(user.tenant_id)
            .bind(id)
            .bind(parent_id)
            .execute(&mut *tx)
            .await
            .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    if res.rows_affected() == 0 {
        return Err(Error::NotFound(format!("parent {parent_id} of role {id}")).into());
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Validate that a condition is a parseable bounded-DSL expression.
fn parse_condition(cond: &serde_json::Value) -> ApiResult<()> {
    serde_json::from_value::<mda_expression::Expr>(cond.clone())
        .map_err(|e| Error::Invalid(format!("condition is not a valid expression: {e}")).into())
        .map(|_| ())
}
