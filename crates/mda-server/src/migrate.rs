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

    // Fail fast — with an actionable message — on the one topology where the
    // migration chain cannot succeed: a migrating role that can neither create
    // the optional `mda_app` role nor rely on it existing, facing migration
    // 20260122000001's unconditional `GRANT … TO mda_app` (see
    // docs/HARDENING.md, sixth pass). Without this the operator gets a bare
    // sqlx "role \"mda_app\" does not exist" buried mid-chain.
    let has_app_role: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_roles WHERE rolname = 'mda_app')")
            .fetch_one(pool)
            .await
            .unwrap_or(false);
    if !has_app_role {
        let createrole: bool = sqlx::query_scalar(
            "SELECT COALESCE(bool(rolcreaterole), false) FROM pg_roles WHERE rolname = current_user",
        )
        .fetch_one(pool)
        .await
        .unwrap_or(false);
        if !createrole {
            // A missing row means the table (or DB) is fresh → still pending.
            let already_applied: Option<i64> = sqlx::query_scalar(
                "SELECT version FROM _sqlx_migrations WHERE version = 20260122000001",
            )
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();
            if already_applied.is_none() {
                anyhow::bail!(
                    "cannot run first-boot migrations: the connecting role lacks CREATEROLE \
                     and the optional 'mda_app' role does not exist, but migration \
                     20260122000001 grants EXECUTE to it unconditionally. Fix: pre-create the \
                     'mda_app' role (release deployments need it anyway — see \
                     MDA_APP_DATABASE_URL in .env.example and docs/HARDENING.md), or grant \
                     CREATEROLE to the migrating role."
                );
            }
        }
    }
    // Path is relative to this crate's `Cargo.toml` (CARGO_MANIFEST_DIR),
    // i.e. `crates/mda-server` → workspace root `migrations/`.
    sqlx::migrate!("../../migrations")
        .run(pool)
        .await
        .context("database migration failed")?;

    // Optional operator rotation of the app-role credential: when
    // MDA_APP_DB_PASSWORD is set, align the cluster role with it so deploy
    // envs don't ship the migration-time default ('mda'). Idempotent; failures
    // surface as startup errors (a wrong password here means the app role
    // can't log in either).
    if let Ok(pw) = std::env::var("MDA_APP_DB_PASSWORD") {
        if !pw.is_empty() {
            // ALTER ROLE takes a string literal, not a parameter — quote it
            // (double any single quotes;Postgres escape rules for literals).
            let escaped = pw.replace('\'', "''");
            sqlx::query(&format!("ALTER ROLE mda_app PASSWORD '{escaped}'"))
                .execute(pool)
                .await
                .context("applying MDA_APP_DB_PASSWORD to the mda_app role")?;
            tracing::info!("mda_app role password applied from MDA_APP_DB_PASSWORD");
        }
    }
    Ok(())
}
