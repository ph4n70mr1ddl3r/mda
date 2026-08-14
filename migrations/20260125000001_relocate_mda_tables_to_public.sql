-- Move platform tables that accidentally landed in the `mda` schema into
-- `public`, where the application code expects them (alongside sys_audit_log,
-- sys_outbox, sys_blob, sys_notification).
--
-- Root cause: the app/CI database role is named `mda`. Postgres's `"$user"`
-- search_path entry expands to a schema named after the *current role*, so when
-- these tables were created with an *unqualified* name (`CREATE TABLE
-- sys_event_log …`) while connected as role `mda`, they landed in the `mda`
-- schema — not `public` as intended. The non-superuser app role `mda_app` has no
-- `mda` in its search_path and no privileges there, so it could not see them and
-- every feature writing to them (event log, secrets, notifications, webhooks,
-- integration runs, external-id registry) silently failed.
--
-- Move them home and grant `mda_app` access. No FKs reference these tables and
-- none carry RLS (they are app-layer tenant-filtered by `tenant_id`), so the
-- move is mechanical: ALTER TABLE … SET SCHEMA moves the table, its indexes,
-- triggers, RLS policies, and owned sequences together. Idempotent.
DO $$
DECLARE t record;
BEGIN
    FOR t IN SELECT tablename FROM pg_tables WHERE schemaname = 'mda' LOOP
        EXECUTE format('ALTER TABLE mda.%I SET SCHEMA public', t.tablename);
    END LOOP;
END
$$;

-- Grant the app role full DML on the relocated tables (the earlier ALL TABLES IN
-- SCHEMA public grant only covered rows present at *that* migration's time, and
-- these were in `mda` then). ALTER DEFAULT PRIVILEGES already covers future
-- tables, so this only closes the gap for the moved set.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'mda_app') THEN
        GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO mda_app;
        GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO mda_app;
    END IF;
END
$$;
