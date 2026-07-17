//! Server-side sessions backing revocable refresh tokens (PLAN §3).
//!
//! One row in `sec.sec_session` per login. The refresh token carries the session
//! id as its `sid` claim; `/api/auth/refresh` rotates it (revoke old + mint new)
//! and detects reuse — a refresh presented for an already-revoked session
//! revokes ALL of the user's sessions (refresh-token-theft containment).
//! `/api/auth/logout` revokes the session named by the access token's `sid`.
//! Access tokens themselves stay stateless (15 m): revocation takes effect on
//! the next refresh, and at the latest within the access TTL.
//!
//! All access is under the tenant GUC (`sec_session` is RLS-gated, tenant-scoped).

use chrono::Duration;
use mda_core::{Error, Result};
use sqlx::PgPool;
use uuid::Uuid;

/// Create a fresh session row under the tenant GUC and return its id.
pub async fn create(
    pool: &PgPool,
    tenant: Uuid,
    user: Uuid,
    ttl: Duration,
    ip: Option<&str>,
) -> Result<Uuid> {
    let id = Uuid::new_v4();
    let mut tx = pool.begin().await.map_err(Error::internal)?;
    crate::set_tenant(&mut tx, tenant).await?;
    sqlx::query(
        "INSERT INTO sec.sec_session (id, tenant_id, user_id, expires_at, ip)
         VALUES ($1, $2, $3, now() + make_interval(secs => $4), $5)",
    )
    .bind(id)
    .bind(tenant)
    .bind(user)
    .bind(ttl.num_seconds() as f64)
    .bind(ip)
    .execute(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(id)
}

/// Outcome of [`rotate`].
pub enum RotateOutcome {
    /// The old session was active and has been replaced by this new session id.
    Rotated(Uuid),
    /// The presented session was already revoked/expired/missing — likely theft.
    /// Every session for the user has been revoked as a precaution.
    Stale,
}

/// Rotate a refresh session atomically (`SELECT … FOR UPDATE`): if `sid` is
/// active, revoke it and mint a new one ([`RotateOutcome::Rotated`]); otherwise
/// revoke all the user's sessions and return [`RotateOutcome::Stale`].
pub async fn rotate(
    pool: &PgPool,
    tenant: Uuid,
    user: Uuid,
    sid: Uuid,
    ttl: Duration,
) -> Result<RotateOutcome> {
    let mut tx = pool.begin().await.map_err(Error::internal)?;
    crate::set_tenant(&mut tx, tenant).await?;
    let active: Option<(Uuid,)> = sqlx::query_as(
        "SELECT id FROM sec.sec_session
          WHERE id = $1 AND tenant_id = $2 AND user_id = $3
            AND revoked_at IS NULL AND expires_at > now()
          FOR UPDATE",
    )
    .bind(sid)
    .bind(tenant)
    .bind(user)
    .fetch_optional(&mut *tx)
    .await
    .map_err(Error::internal)?;

    if active.is_some() {
        let new_id = Uuid::new_v4();
        sqlx::query("UPDATE sec.sec_session SET revoked_at = now() WHERE id = $1")
            .bind(sid)
            .execute(&mut *tx)
            .await
            .map_err(Error::internal)?;
        sqlx::query(
            "INSERT INTO sec.sec_session (id, tenant_id, user_id, expires_at)
             VALUES ($1, $2, $3, now() + make_interval(secs => $4))",
        )
        .bind(new_id)
        .bind(tenant)
        .bind(user)
        .bind(ttl.num_seconds() as f64)
        .execute(&mut *tx)
        .await
        .map_err(Error::internal)?;
        tx.commit().await.map_err(Error::internal)?;
        Ok(RotateOutcome::Rotated(new_id))
    } else {
        // Reuse / stale refresh → revoke everything for the user (theft containment).
        sqlx::query(
            "UPDATE sec.sec_session SET revoked_at = now()
              WHERE user_id = $1 AND tenant_id = $2 AND revoked_at IS NULL",
        )
        .bind(user)
        .bind(tenant)
        .execute(&mut *tx)
        .await
        .map_err(Error::internal)?;
        tx.commit().await.map_err(Error::internal)?;
        Ok(RotateOutcome::Stale)
    }
}

/// Revoke a single session (logout). No-op if already revoked or absent.
pub async fn revoke(pool: &PgPool, tenant: Uuid, sid: Uuid) -> Result<()> {
    let mut tx = pool.begin().await.map_err(Error::internal)?;
    crate::set_tenant(&mut tx, tenant).await?;
    sqlx::query(
        "UPDATE sec.sec_session SET revoked_at = now()
          WHERE id = $1 AND tenant_id = $2 AND revoked_at IS NULL",
    )
    .bind(sid)
    .bind(tenant)
    .execute(&mut *tx)
    .await
    .map_err(Error::internal)?;
    tx.commit().await.map_err(Error::internal)?;
    Ok(())
}
