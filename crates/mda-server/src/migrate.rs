//! Run the embedded SQLx migrations (`<workspace>/migrations`).

use anyhow::Context;
use sqlx::PgPool;

/// Apply all pending migrations. Idempotent.
pub async fn run(pool: &PgPool) -> anyhow::Result<()> {
    // Path is relative to this crate's `Cargo.toml` (CARGO_MANIFEST_DIR),
    // i.e. `crates/mda-server` → workspace root `migrations/`.
    sqlx::migrate!("../../migrations")
        .run(pool)
        .await
        .context("database migration failed")
}
