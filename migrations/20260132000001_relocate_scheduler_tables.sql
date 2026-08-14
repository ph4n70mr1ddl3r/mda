-- Relocate the scheduler tables into `public`, where the application expects
-- them (mirroring 20260125000001 for the earlier `mda`-schema accident).
--
-- Root cause: migrations/20260126000001 creates `sys_schedule` /
-- `sys_schedule_run` UNQUALIFIED while connected as the owner role `mda`.
-- Postgres's `"$user"` search_path entry expands to the schema named after the
-- current role, and the `mda` schema legitimately exists at that point (it
-- hosts the platform trigger functions, created by 20260110000001) — so the
-- two tables landed in `mda`, not `public`. The non-superuser app role
-- `mda_app` has no `mda` in its search_path, so every scheduler query failed
-- with "relation \"sys_schedule\" does not exist" in release/staging
-- deployments (dev hides it: `make run-dev` serves as the owner, whose
-- search_path still resolves the names).
--
-- The move is mechanical: ALTER TABLE … SET SCHEMA carries the table's
-- indexes and owned sequences along; these tables carry no RLS (the scheduler
-- worker claims rows across tenants with FOR UPDATE SKIP LOCKED, like the
-- outbox drain) and nothing references them by FK. Idempotent: databases
-- where the tables already sit in `public` simply skip the loop.
DO $$
DECLARE t record;
BEGIN
    FOR t IN SELECT tablename FROM pg_tables
             WHERE schemaname = 'mda'
               AND tablename IN ('sys_schedule', 'sys_schedule_run')
    LOOP
        EXECUTE format('ALTER TABLE mda.%I SET SCHEMA public', t.tablename);
    END LOOP;
END
$$;

-- Grant the app role full DML on the relocated tables (the ALL TABLES IN
-- SCHEMA public grant in earlier migrations only covered rows present at
-- those migrations' time). ALTER DEFAULT PRIVILEGES covers future tables.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'mda_app') THEN
        GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO mda_app;
        GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO mda_app;
    END IF;
END
$$;
