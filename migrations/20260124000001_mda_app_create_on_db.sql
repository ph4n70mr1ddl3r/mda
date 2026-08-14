-- Grant `CREATE` on the database to `mda_app`.
--
-- Why: the `biz` schema and its tables are created *at publish time* by the app
-- when it runs as the non-superuser `mda_app` role (see mda-data::ddl::ensure_schema:
-- `CREATE SCHEMA IF NOT EXISTS biz`). PostgreSQL checks the `CREATE` privilege on
-- the database for any `CREATE SCHEMA` statement (even with `IF NOT EXISTS`, when
-- the schema already exists), so without this grant publish DDL fails with
-- `permission denied for database <db>`.
--
-- The earlier RLS migration (20260111) granted only `CONNECT` here, which left the
-- publish path broken whenever the app connected as `mda_app` — i.e. in every
-- non-superuser deployment and in the per-test-database suites (data / studio).
-- This closes that gap. Idempotent and a no-op where `mda_app` does not exist.

DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'mda_app') THEN
        EXECUTE format('GRANT CONNECT, CREATE ON DATABASE %I TO mda_app', current_database());
        RAISE NOTICE 'granted CONNECT, CREATE on database % to mda_app', current_database();
    END IF;
END
$$;
