//! Run the embedded SQLx migrations (`<workspace>/migrations`).

use anyhow::Context;
use sqlx::PgPool;

/// Apply all pending migrations. Idempotent.
pub async fn run(pool: &PgPool) -> anyhow::Result<()> {
    // Pre-create the global `mda_app` role, tolerating a concurrent winner.
    // Roles live in cluster-global `pg_authid`, but migrations run per
    // database — parallel test databases (and any concurrent first-boot
    // migrations) can race inside 20260111000001's check-then-create and lose
    // with `duplicate key value violates unique constraint
    // "pg_authid_rolname_index"`. Creating it here first makes that check
    // always find the role. Failures are deliberately swallowed: deployments
    // that connect as an existing non-superuser role don't need it, and
    // migration 11 re-attempts (with the same semantics) if this no-ops.
    sqlx::query(
        "DO $$
         BEGIN
             IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'mda_app') THEN
                 CREATE ROLE mda_app LOGIN PASSWORD 'mda' NOBYPASSRLS;
             END IF;
         EXCEPTION
             WHEN unique_violation OR insufficient_privilege OR insufficient_resources THEN
                 RAISE NOTICE 'mda_app pre-create skipped (%)', SQLERRM;
         END $$",
    )
    .execute(pool)
    .await
    .ok();
    // Path is relative to this crate's `Cargo.toml` (CARGO_MANIFEST_DIR),
    // i.e. `crates/mda-server` → workspace root `migrations/`.
    sqlx::migrate!("../../migrations")
        .run(pool)
        .await
        .context("database migration failed")
}
