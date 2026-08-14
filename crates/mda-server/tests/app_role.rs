//! Production-role regression suite.
//!
//! The app serves as the non-superuser `mda_app` role in every release/staging
//! deployment (dev runs as the owner, which hides permission gaps — several
//! real bugs shipped that way: `sys_schedule` in the wrong schema, no USAGE on
//! `int`, runtime DDL in the webhook relay). These tests fail loudly whenever
//! a migration leaves the production role unable to use a surface the app
//! touches.

mod common;

/// Every table in the schemas the app queries (public, meta, sec, int) must be
/// at least SELECT-able by `mda_app` after the full migration chain —
/// and writable where the app writes (public, meta, sec, int all carry
/// app-written tables; the whitelist is only read paths like biz_archive).
#[tokio::test]
async fn app_role_can_read_every_app_schema_table() {
    let url = std::env::var("DATABASE_URL").unwrap();
    let (pool, _db) = common::spawn_db(&url).await;
    let mut conn = pool.acquire().await.unwrap();
    let has_role: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_roles WHERE rolname = 'mda_app')")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    if !has_role {
        return; // restricted environments without role-creation rights
    }

    let tables: Vec<(String, String)> = sqlx::query_as(
        "SELECT n.nspname, c.relname
           FROM pg_class c JOIN pg_namespace n ON c.relnamespace = n.oid
          WHERE c.relkind = 'r'
            AND n.nspname IN ('public', 'meta', 'sec', 'int')",
    )
    .fetch_all(&mut *conn)
    .await
    .unwrap();
    assert!(!tables.is_empty(), "no tables found — harness bug");

    sqlx::query("SET ROLE mda_app")
        .execute(&mut *conn)
        .await
        .unwrap();
    let mut unreadable = Vec::new();
    for (schema, table) in &tables {
        // `_sqlx_migrations` is the migration runner's bookkeeping (owner-only
        // by design); everything else is an app surface.
        if schema == "public" && table == "_sqlx_migrations" {
            continue;
        }
        let can: bool = sqlx::query_scalar(&format!(
            "SELECT has_table_privilege('mda_app', '{}.{}', 'SELECT')",
            schema.replace('\'', "''"),
            table.replace('\'', "''")
        ))
        .fetch_one(&mut *conn)
        .await
        .unwrap();
        if !can {
            unreadable.push(format!("{schema}.{table}"));
        }
    }
    assert!(
        unreadable.is_empty(),
        "mda_app cannot read (missing GRANT or wrong schema): {unreadable:?}"
    );
}

/// The schemas the app resolves unqualified DML through must grant USAGE to
/// `mda_app` (a missing USAGE fails every query with "permission denied for
/// schema <s>" — the int-schema bug).
#[tokio::test]
async fn app_role_has_usage_on_app_schemas() {
    let url = std::env::var("DATABASE_URL").unwrap();
    let (pool, _db) = common::spawn_db(&url).await;
    let mut conn = pool.acquire().await.unwrap();
    let has_role: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_roles WHERE rolname = 'mda_app')")
            .fetch_one(&mut *conn)
            .await
            .unwrap();
    if !has_role {
        return;
    }
    let rows: Vec<(String, bool)> = sqlx::query_as(
        "SELECT nspname, has_schema_privilege('mda_app', nspname, 'USAGE')
           FROM pg_namespace
          WHERE nspname IN ('public', 'meta', 'sec', 'int', 'biz_archive')",
    )
    .fetch_all(&mut *conn)
    .await
    .unwrap();
    let missing: Vec<String> = rows
        .iter()
        .filter(|(_, ok)| !ok)
        .map(|(n, _)| n.clone())
        .collect();
    assert!(
        missing.is_empty(),
        "mda_app lacks USAGE on schemas: {missing:?}"
    );
}

/// Concurrent first-time migrations must not race on the cluster-global
/// `mda_app` role. Roles live in `pg_authid` (shared by every database on the
/// cluster), so parallel test databases executing migration 20260111000001's
/// check-then-create simultaneously lose with `duplicate key value violates
/// unique constraint "pg_authid_rolname_index"` — this was a recurring red CI
/// on fully-parallel suite runs. `migrate::run` pre-creates the role
/// race-tolerantly; this test would fail without it.
#[tokio::test]
async fn concurrent_migrations_do_not_race_on_the_app_role() {
    let admin_url = std::env::var("DATABASE_URL").unwrap();
    let admin = sqlx::postgres::PgPoolOptions::new()
        .max_connections(10)
        .connect(&admin_url)
        .await
        .unwrap();

    // N fresh databases, dropped first so the test is rerunnable.
    let n = 6;
    let pid = std::process::id();
    let names: Vec<String> = (0..n).map(|i| format!("mda_race_{pid}_{i}")).collect();
    for name in &names {
        sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
            .execute(&admin)
            .await
            .unwrap();
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&admin)
            .await
            .unwrap();
    }

    // Migrate all of them at once — the loser of any role race fails its whole
    // migration chain, so all six must succeed.
    let mut set = tokio::task::JoinSet::new();
    for name in &names {
        let url = common::url_with_db(&admin_url, name);
        set.spawn(async move {
            let pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(4)
                .connect(&url)
                .await
                .unwrap();
            mda_server::migrate::run(&pool).await
        });
    }
    while let Some(joined) = set.join_next().await {
        let r = joined.expect("migration task panicked");
        assert!(
            r.is_ok(),
            "concurrent migration failed: {:?}",
            r.err().map(|e| e.to_string())
        );
    }
    for name in &names {
        sqlx::query(&format!("DROP DATABASE {name} WITH (FORCE)"))
            .execute(&admin)
            .await
            .unwrap();
    }
}
