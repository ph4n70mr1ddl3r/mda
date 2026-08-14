-- Phase 6 UI definitions (PLAN §7 / Phase 6): md_form, md_view, md_dashboard,
-- md_navigation.
--
-- These are *presentation* metadata (domain-neutral): which fields appear on a
-- rendered form and in what order/widget, which columns a list view shows, what
-- a dashboard tiles, and how the app's navigation is organized. They carry no
-- logic — the runtime API resolves them against the ACTIVE model and the
-- CALLER's security (a field the caller cannot read never reaches a form/view
-- payload; a dashboard runs its reports under the requesting identity), so a
-- UI definition can never widen access.
--
-- layout / columns / items are JSONB documents authored via the API (the future
-- Studio designers write here); see docs/CAPABILITIES.md for the shapes.

CREATE TABLE IF NOT EXISTS meta.md_form (
    id         UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id  UUID        NOT NULL,
    entity     TEXT        NOT NULL,
    name       TEXT        NOT NULL DEFAULT 'default',
    label      TEXT,
    layout     JSONB       NOT NULL DEFAULT '{"sections":[]}'::jsonb,
    active     BOOLEAN     NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, entity, name)
);

CREATE TABLE IF NOT EXISTS meta.md_view (
    id         UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id  UUID        NOT NULL,
    entity     TEXT        NOT NULL,
    name       TEXT        NOT NULL DEFAULT 'default',
    label      TEXT,
    columns    JSONB       NOT NULL DEFAULT '[]'::jsonb,  -- [{field,label,width}]
    filters    JSONB       NOT NULL DEFAULT '[]'::jsonb,  -- ListParams filters
    sort       JSONB       NOT NULL DEFAULT '[]'::jsonb,  -- [{field,asc}]
    page_size  INTEGER,
    active     BOOLEAN     NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, entity, name)
);

-- Dashboards tile saved reports (md_report); the runtime renders each tile by
-- RUNNING the report under the requesting identity (never cached credentials).
CREATE TABLE IF NOT EXISTS meta.md_dashboard (
    id         UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id  UUID        NOT NULL,
    name       TEXT        NOT NULL,
    label      TEXT,
    items      JSONB       NOT NULL DEFAULT '[]'::jsonb,  -- [{report_id,title,span}]
    active     BOOLEAN     NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, name)
);

-- Navigation: an ordered item list (entity links + external links). Entity
-- links are permission-filtered at read time; external URLs are http(s) only.
CREATE TABLE IF NOT EXISTS meta.md_navigation (
    id         UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id  UUID        NOT NULL,
    name       TEXT        NOT NULL DEFAULT 'default',
    label      TEXT,
    items      JSONB       NOT NULL DEFAULT '[]'::jsonb,  -- [{type,entity|url,label}]
    active     BOOLEAN     NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, name)
);

-- RLS — same tenant-isolation policy as the other meta.md_* tables (the
-- generic meta-RLS pass predates these tables, so gate each explicitly).
DO $$
DECLARE t record;
BEGIN
    FOR t IN SELECT tablename FROM pg_tables
              WHERE schemaname='meta'
                AND tablename IN ('md_form','md_view','md_dashboard','md_navigation')
    LOOP
        EXECUTE format('ALTER TABLE meta.%I ENABLE ROW LEVEL SECURITY', t.tablename);
        EXECUTE format('ALTER TABLE meta.%I FORCE ROW LEVEL SECURITY',  t.tablename);
        EXECUTE format(
            'DROP POLICY IF EXISTS tenant_isolation ON meta.%I;
             CREATE POLICY tenant_isolation ON meta.%I
             USING (tenant_id = NULLIF(current_setting(''app.tenant_id'', true), '''')::uuid)
             WITH CHECK (tenant_id = NULLIF(current_setting(''app.tenant_id'', true), '''')::uuid)',
            t.tablename, t.tablename);
    END LOOP;
END
$$;
