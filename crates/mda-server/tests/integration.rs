//! Integration test: applies migrations to the DB named by `DATABASE_URL` and
//! asserts the meta schema is in place. Skipped when `DATABASE_URL` is unset,
//! so `cargo test` still passes without a database.

use sqlx::postgres::PgPoolOptions;

#[tokio::test]
async fn migrations_apply_and_meta_schema_exists() {
    let url = match std::env::var("DATABASE_URL") {
        Ok(u) => u,
        Err(_) => {
            eprintln!("skipping: DATABASE_URL not set");
            return;
        }
    };

    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("connect to DATABASE_URL");

    mda_server::migrate::run(&pool).await.expect("migrate");

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

    // the bootstrap active-version pointer should still be present
    let bootstrap: i64 =
        sqlx::query_scalar("SELECT count(*) FROM meta.md_active_version WHERE tenant_id = '00000000-0000-0000-0000-000000000000'")
            .fetch_one(&pool)
            .await
            .expect("query md_active_version");
    assert!(bootstrap >= 1, "bootstrap active_version row should exist");
}
