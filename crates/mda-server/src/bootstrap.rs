//! Bootstrap: ensure an admin user exists for the bootstrap tenant so the system
//! is usable on first run. Idempotent.

use sqlx::PgPool;
use uuid::Uuid;

/// The bootstrap (all-zeros) tenant — created by the Phase-0 migration seed.
const TENANT: Uuid = Uuid::nil();

/// Ensure a bootstrap admin (`admin@mda.local`) with a superuser role exists.
/// Password from `MDA_BOOTSTRAP_PASSWORD` (default `admin123`).
pub async fn ensure_admin(pool: &PgPool) -> anyhow::Result<()> {
    let exists: Option<(Uuid,)> =
        sqlx::query_as("SELECT id FROM sec.sec_user WHERE tenant_id = $1 LIMIT 1")
            .bind(TENANT)
            .fetch_optional(pool)
            .await?;
    if exists.is_some() {
        return Ok(());
    }

    let password =
        std::env::var("MDA_BOOTSTRAP_PASSWORD").unwrap_or_else(|_| "admin123".to_string());
    let hash = mda_security::hash_password(&password)?;

    let (team_id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO sec.sec_team (tenant_id, name) VALUES ($1, 'Admins')
         ON CONFLICT (tenant_id, name) DO UPDATE SET name = 'Admins' RETURNING id",
    )
    .bind(TENANT)
    .fetch_one(pool)
    .await?;
    let (role_id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO sec.sec_role (tenant_id, name) VALUES ($1, 'admin')
         ON CONFLICT (tenant_id, name) DO UPDATE SET name = 'admin' RETURNING id",
    )
    .bind(TENANT)
    .fetch_one(pool)
    .await?;
    sqlx::query(
        "INSERT INTO sec.sec_permission (role_id, entity, verb) VALUES ($1, '*', '*')
         ON CONFLICT DO NOTHING",
    )
    .bind(role_id)
    .execute(pool)
    .await?;
    let (user_id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO sec.sec_user (tenant_id, team_id, email, name, password_hash)
         VALUES ($1, $2, 'admin@mda.local', 'Administrator', $3)
         ON CONFLICT (tenant_id, email) DO UPDATE SET password_hash = $3 RETURNING id",
    )
    .bind(TENANT)
    .bind(team_id)
    .bind(&hash)
    .fetch_one(pool)
    .await?;
    sqlx::query(
        "INSERT INTO sec.sec_role_assignment (user_id, role_id) VALUES ($1, $2)
         ON CONFLICT DO NOTHING",
    )
    .bind(user_id)
    .bind(role_id)
    .execute(pool)
    .await?;

    tracing::info!("bootstrap admin ready: admin@mda.local (tenant {TENANT})");
    Ok(())
}
