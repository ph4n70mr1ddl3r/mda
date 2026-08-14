-- Pin the database search_path to `public`, closing the `"$user"` capture
-- class for good.
--
-- Background: the app/CI database role is named `mda`, and the platform keeps
-- a schema literally named `mda` (trigger functions; created by
-- 20260110000001). Postgres's default search_path starts with `"$user"`,
-- which expands to the schema named after the current role — so EVERY
-- unqualified CREATE/lookup as role `mda` resolved into that schema whenever
-- it existed. That single quirk caused, at various times: platform tables
-- landing in `mda` instead of `public` (relocated by 20260125000001 and
-- 20260132000001), the scheduler tables landing there again (20260126),
-- and even sqlx's own `_sqlx_migrations` bookkeeping table being created
-- there on databases that already had the schema — which made the migration
-- history look empty and triggered a full re-run.
--
-- Application references to `meta.*`, `sec.*`, `biz.*`, `int.*` and the
-- `mda.*` trigger functions are schema-qualified; the app role `mda_app` never
-- matched `"$user"` anyway. Pinning to `public` changes nothing for correct
-- deployments and makes every future unqualified DDL (migration 35+, sqlx
-- bookkeeping on recovered databases) resolve deterministically to `public`.
--
-- Applies to NEW connections on the database being migrated; ALTER DATABASE
-- … SET is transaction-safe (it only writes pg_db_role_setting).
DO $$
BEGIN
    EXECUTE format('ALTER DATABASE %I SET search_path TO public', current_database());
END
$$;
