//! `mda-security` — authentication & authorization (PLAN §5.11).
//!
//! Phase 3 implements the **tenant, object, field, record(ownership/OWD)**
//! grains. Record sharing rules / role hierarchy / materialized `sec_record_share`
//! arrive in Phase 6 (ADR-0013); Postgres RLS is a defense-in-depth follow-up.

pub mod context;
pub mod identity;
pub mod jwt;
pub mod login_throttle;
pub mod password;
pub mod session;

pub use context::{load_identity, resolve_owd};
pub use identity::{Access, Identity, Owd};
pub use jwt::{AccessToken, JwtConfig};
pub use login_throttle::LoginThrottle;
pub use password::{hash_password, verify_password};

/// Set the per-transaction tenant context used by the `sec.*` RLS policies
/// (PLAN §5.4 / §5.11). `set_config(..., true)` is transaction-local; without it
/// the policy denies every row (fail-closed). Callers wrap their `sec.*` query
/// in a transaction begun immediately before this.
pub async fn set_tenant(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant: uuid::Uuid,
) -> mda_core::Result<()> {
    sqlx::query("SELECT set_config('app.tenant_id', $1, true)")
        .bind(tenant.to_string())
        .execute(&mut **tx)
        .await
        .map_err(mda_core::Error::internal)?;
    Ok(())
}
