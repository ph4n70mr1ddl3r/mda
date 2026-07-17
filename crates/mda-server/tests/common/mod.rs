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
