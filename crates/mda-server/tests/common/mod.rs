//! Shared test helper: an isolated PostgreSQL database per test.
//!
//! Each test gets its own fresh database (created + migrated on the fly) so the
//! DB-backed suites are fully parallel-safe — no shared state, no `--test-threads=1`.
//! Stale databases from previous runs (different process id) are dropped first;
//! the current run's databases are named with this process's pid so parallel
//! siblings never drop each other.
//!
//! Requires the `DATABASE_URL` role to have `CREATEDB`. (The dev/CI `mda` user
//! is a superuser; a local non-superuser with CREATEDB works too.)

// Compiled into each test binary; not every helper is used by every binary.
#![allow(dead_code)]

use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use uuid::Uuid;

/// Create + migrate a fresh `mda_test_<pid>_<rand>` database and return a pool
/// to it plus its connection URL. Panics on failure (tests).
pub async fn spawn_db(admin_url: &str) -> (PgPool, String) {
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect(admin_url)
        .await
        .expect("connect to DATABASE_URL (admin)");

    let pid = std::process::id();
    cleanup_other_runs(&admin, pid).await;

    let name = format!("mda_test_{pid}_{}", Uuid::new_v4().simple());
    // CREATE/DROP DATABASE cannot be parameterised or run in a txn.
    sqlx::query(&format!("CREATE DATABASE {name}"))
        .execute(&admin)
        .await
        .expect("create test database");

    let url = url_with_db(admin_url, &name);
    let pool = PgPoolOptions::new()
        .max_connections(6)
        .connect(&url)
        .await
        .expect("connect to test database");
    mda_server::migrate::run(&pool)
        .await
        .expect("migrate test database");
    (pool, url)
}

/// Drop `mda_test_%` databases from *other* process ids (previous runs). Same-pid
/// databases (this run's siblings) are spared. `WITH (FORCE)` handles any stray
/// connection (Postgres 13+).
async fn cleanup_other_runs(admin: &PgPool, this_pid: u32) {
    let keep_like = format!("mda_test_{this_pid}_%");
    // datname is from pg_database (LIKE 'mda_test_%'); safe to interpolate.
    let stale: Vec<String> = sqlx::query_scalar(
        "SELECT datname FROM pg_database
          WHERE datname LIKE 'mda_test_%' AND datname NOT LIKE $1",
    )
    .bind(keep_like)
    .fetch_all(admin)
    .await
    .unwrap_or_default();
    for name in stale {
        let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
            .execute(admin)
            .await;
    }
}

/// Replace the database name (trailing path segment) in a Postgres URL.
/// `postgres:///mda` → `postgres:///mda_test_x`; `postgres://u:p@h:5432/mda` → `…/mda_test_x`.
fn url_with_db(url: &str, db: &str) -> String {
    match url.rfind('/') {
        Some(pos) => format!("{}{db}", &url[..pos + 1]),
        None => format!("/{db}"),
    }
}

/// Insert a `sec_user` under the tenant GUC (`sec_user` is RLS-gated, so a
/// GUC-less insert is rejected by the WITH CHECK policy). Returns the new id.
pub async fn seed_user(
    pool: &sqlx::PgPool,
    tenant: uuid::Uuid,
    email: &str,
    name: &str,
    password_hash: &str,
) -> uuid::Uuid {
    let mut tx = pool.begin().await.expect("seed_user begin");
    mda_security::set_tenant(&mut tx, tenant)
        .await
        .expect("seed_user set_tenant");
    let (id,): (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO sec.sec_user (tenant_id, email, name, password_hash)
         VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(tenant)
    .bind(email)
    .bind(name)
    .bind(password_hash)
    .fetch_one(&mut *tx)
    .await
    .expect("seed_user insert");
    tx.commit().await.expect("seed_user commit");
    id
}

/// Create a role + its (entity, verb) permissions under the tenant GUC. sec_role
/// is RLS-gated; sec_permission's tenant_id is auto-filled by trigger from the
/// role (which the GUC makes visible). Returns the role id.
pub async fn seed_role(
    pool: &sqlx::PgPool,
    tenant: uuid::Uuid,
    name: &str,
    perms: &[(&str, &str)],
) -> uuid::Uuid {
    let mut tx = pool.begin().await.expect("seed_role begin");
    mda_security::set_tenant(&mut tx, tenant)
        .await
        .expect("seed_role set_tenant");
    let (role_id,): (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO sec.sec_role (tenant_id, name) VALUES ($1, $2)
         ON CONFLICT (tenant_id, name) DO UPDATE SET name = $2 RETURNING id",
    )
    .bind(tenant)
    .bind(name)
    .fetch_one(&mut *tx)
    .await
    .expect("seed_role insert");
    for (e, v) in perms {
        sqlx::query(
            "INSERT INTO sec.sec_permission (role_id, entity, verb) VALUES ($1, $2, $3)
             ON CONFLICT DO NOTHING",
        )
        .bind(role_id)
        .bind(e)
        .bind(v)
        .execute(&mut *tx)
        .await
        .expect("seed_role permission");
    }
    tx.commit().await.expect("seed_role commit");
    role_id
}

/// Assign a role to a user under the tenant GUC (sec_role_assignment is
/// RLS-gated; its tenant_id is auto-filled by trigger from the role).
pub async fn seed_assignment(
    pool: &sqlx::PgPool,
    tenant: uuid::Uuid,
    user_id: uuid::Uuid,
    role_id: uuid::Uuid,
) {
    let mut tx = pool.begin().await.expect("seed_assignment begin");
    mda_security::set_tenant(&mut tx, tenant)
        .await
        .expect("seed_assignment set_tenant");
    sqlx::query(
        "INSERT INTO sec.sec_role_assignment (user_id, role_id) VALUES ($1, $2)
         ON CONFLICT DO NOTHING",
    )
    .bind(user_id)
    .bind(role_id)
    .execute(&mut *tx)
    .await
    .expect("seed_assignment insert");
    tx.commit().await.expect("seed_assignment commit");
}
