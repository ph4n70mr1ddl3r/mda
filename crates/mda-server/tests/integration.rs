//! Integration test: applies migrations to a fresh per-test database and asserts
//! the meta schema is in place. Skipped when `DATABASE_URL` is unset, so
//! `cargo test` still passes without a database.

mod common;

#[tokio::test]
async fn migrations_apply_and_meta_schema_exists() {
    let url = match std::env::var("DATABASE_URL") {
        Ok(u) => u,
        Err(_) => {
            eprintln!("skipping: DATABASE_URL not set");
            return;
        }
    };
    let (pool, _db_url) = common::spawn_db(&url).await;

    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM information_schema.tables
          WHERE table_schema = 'meta' AND table_name IN
                ('md_module','md_entity','md_field','md_relationship',
                 'md_active_version','md_draft','md_snapshot','md_migration_log','md_retirement')",
    )
    .fetch_one(&pool)
    .await
    .expect("query meta tables");
    assert_eq!(count, 9, "all meta skeleton tables should exist");

    // The bootstrap active-version pointer is seeded by a migration (not by
    // bootstrap::ensure_admin), so a freshly migrated DB already has it.
    let bootstrap: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM meta.md_active_version \
          WHERE tenant_id = '00000000-0000-0000-0000-000000000000'",
    )
    .fetch_one(&pool)
    .await
    .expect("query md_active_version");
    assert_eq!(
        bootstrap, 1,
        "bootstrap active_version row should be seeded by migration"
    );
}
