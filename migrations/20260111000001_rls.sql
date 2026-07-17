-- Row-Level Security: the DB-layer tenant-isolation backstop (PLAN §5.4 / §5.11).
--
-- The app layer already filters every query by tenant_id; RLS is the
-- defense-in-depth that catches a query which forgets. This migration sets up
-- the non-superuser role the app should connect as in production (superusers
-- BYPASS RLS, so the owner/superuser cannot be the app role) and grants it the
-- privileges it needs.
--
-- What engages RLS is the combination of ENABLE+FORCE policies on the biz
-- tables (below + in mda-data::ddl at publish) AND the app connecting as a
-- NON-SUPERUSER role. In production that role is `mda_app`; in any deployment
-- where the app already connects as a non-superuser, the `mda_app` role is
-- optional and its creation is skipped if the migrating role lacks CREATEROLE.
--
-- Scope of THIS pass: `biz.*` and `biz_archive.*` (dynamic business data,
-- created at publish). Background workers touch only `sys_*` / `meta` tables
-- (no RLS here), so they are unaffected and need no exemption. `sec.*` /
-- `meta.*` remain app-layer-isolated; the same pattern extends to them later.

-- ===== the app role (non-superuser, no BYPASSRLS) — optional =====
-- Created only if the migrating role may create roles. Never resets an existing
-- role/password. Dev default password 'mda'.
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'mda_app') THEN
        CREATE ROLE mda_app LOGIN PASSWORD 'mda' NOBYPASSRLS;
        RAISE NOTICE 'created role mda_app';
    END IF;
EXCEPTION
    WHEN insufficient_privilege OR insufficient_resources THEN
        RAISE NOTICE 'mda_app not created (current role lacks CREATEROLE); the app must connect as a non-superuser role for RLS to engage';
END
$$;

-- Grants to mda_app — only if it exists. The `biz` schema and its tables are
-- created at publish time BY mda_app, so it owns them and needs no grant here;
-- these cover the migration-owned `meta`/`sec` tables and the `sys_*`
-- operational tables. Those sys_* tables live in `public` (there is no separate
-- `sys` schema), so grant against `public` — the old `IN SCHEMA sys` references
-- a non-existent schema and made this whole block fail whenever mda_app existed
-- (i.e. any deployment that actually sets MDA_APP_DATABASE_URL).
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'mda_app') THEN
        GRANT USAGE ON SCHEMA meta, sec, mda, biz_archive TO mda_app;
        -- mda_app creates the twin archive tables at publish → needs CREATE there.
        GRANT USAGE, CREATE ON SCHEMA biz_archive TO mda_app;
        GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA meta, sec, public TO mda_app;
        GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA meta, public TO mda_app;
        GRANT EXECUTE ON ALL FUNCTIONS IN SCHEMA meta, sec, mda TO mda_app;
        -- Future tables/sequences created by the migration role in these schemas
        -- are granted automatically, so a later sys_* table (e.g.
        -- sys_login_throttle) is covered without each migration repeating a GRANT.
        ALTER DEFAULT PRIVILEGES IN SCHEMA meta, sec, public
            GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO mda_app;
        ALTER DEFAULT PRIVILEGES IN SCHEMA meta, sec, public
            GRANT USAGE, SELECT ON SEQUENCES TO mda_app;
        EXECUTE format('GRANT CONNECT ON DATABASE %I TO mda_app', current_database());
    END IF;
END
$$;

-- ===== retrofit RLS onto any biz / biz_archive tables that already exist =====
-- (New ones get ENABLE+FORCE+policy from mda-data::ddl at publish. This block
-- covers tables created before RLS shipped — a no-op on a fresh DB.)
DO $$
DECLARE
    t record;
BEGIN
    FOR t IN
        SELECT schemaname, tablename
          FROM pg_tables
         WHERE schemaname IN ('biz', 'biz_archive')
    LOOP
        EXECUTE format('ALTER TABLE %I.%I ENABLE ROW LEVEL SECURITY', t.schemaname, t.tablename);
        EXECUTE format('ALTER TABLE %I.%I FORCE ROW LEVEL SECURITY',  t.schemaname, t.tablename);
        EXECUTE format(
            'DROP POLICY IF EXISTS tenant_isolation ON %I.%I;
             CREATE POLICY tenant_isolation ON %I.%I
             USING (tenant_id = NULLIF(current_setting(''app.tenant_id'', true), '''')::uuid)
             WITH CHECK (tenant_id = NULLIF(current_setting(''app.tenant_id'', true), '''')::uuid)',
            t.schemaname, t.tablename, t.schemaname, t.tablename
        );
    END LOOP;
END
$$;
