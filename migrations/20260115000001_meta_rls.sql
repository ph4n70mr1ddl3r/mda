-- RLS on meta.* (model definitions). Completes tenant isolation coverage.
--
-- All meta.md_* tables are tenant-scoped and read/written only in request context,
-- EXCEPT md_active_version: the cache poller (mda-meta::cache::spawn_poll) reads
-- it ACROSS tenants to detect version advances, so it must stay RLS-free.
--
-- md_workflow_state / md_workflow_transition are children of md_workflow and lack
-- tenant_id; like the sec role-keyed tables, denormalise tenant_id from the
-- parent (md_workflow) via a BEFORE INSERT/UPDATE trigger, then gate everything.

ALTER TABLE meta.md_workflow_state      ADD COLUMN IF NOT EXISTS tenant_id UUID;
ALTER TABLE meta.md_workflow_transition ADD COLUMN IF NOT EXISTS tenant_id UUID;

UPDATE meta.md_workflow_state      ws SET tenant_id = w.tenant_id FROM meta.md_workflow w WHERE w.id = ws.workflow_id      AND ws.tenant_id IS NULL;
UPDATE meta.md_workflow_transition tr SET tenant_id = w.tenant_id FROM meta.md_workflow w WHERE w.id = tr.workflow_id      AND tr.tenant_id IS NULL;

CREATE OR REPLACE FUNCTION mda.meta_tenant_from_workflow() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    SELECT w.tenant_id INTO NEW.tenant_id FROM meta.md_workflow w WHERE w.id = NEW.workflow_id;
    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS md_workflow_state_tenant      ON meta.md_workflow_state;
CREATE TRIGGER md_workflow_state_tenant      BEFORE INSERT OR UPDATE ON meta.md_workflow_state
    FOR EACH ROW EXECUTE FUNCTION mda.meta_tenant_from_workflow();
DROP TRIGGER IF EXISTS md_workflow_transition_tenant ON meta.md_workflow_transition;
CREATE TRIGGER md_workflow_transition_tenant BEFORE INSERT OR UPDATE ON meta.md_workflow_transition
    FOR EACH ROW EXECUTE FUNCTION mda.meta_tenant_from_workflow();

-- Gate every meta.md_* table except md_active_version (cache poller) and the
-- internal _sqlx_migrations (sqlx owns it). Idempotent: skips tables already gated.
DO $$
DECLARE t record;
BEGIN
    FOR t IN SELECT tablename FROM pg_tables
              WHERE schemaname='meta'
                AND tablename <> 'md_active_version'
                AND tablename NOT IN (
                    SELECT relname FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
                     WHERE n.nspname='meta' AND c.relrowsecurity
                )
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
