-- RLS on the remaining sec.* tables (completes sec coverage).
--
-- sec_team / sec_role / sec_role_hierarchy / sec_share_rule already carry
-- tenant_id → gated directly.
--
-- sec_permission / sec_field_permission / sec_role_assignment are keyed by
-- role_id with NO tenant_id. Rather than a recursive subquery policy (fragile
-- and slow once sec_role is itself RLS-gated), denormalise tenant_id onto them
-- and keep it in sync with a BEFORE INSERT/UPDATE trigger that copies it from
-- the role. App code keeps inserting by role_id only; the trigger fills
-- tenant_id, and the WITH CHECK policy validates it equals the GUC (fail-closed
-- if the role isn't visible under the current tenant).

-- (1) add + backfill tenant_id on the role-keyed tables
ALTER TABLE sec.sec_permission        ADD COLUMN IF NOT EXISTS tenant_id UUID;
ALTER TABLE sec.sec_field_permission  ADD COLUMN IF NOT EXISTS tenant_id UUID;
ALTER TABLE sec.sec_role_assignment   ADD COLUMN IF NOT EXISTS tenant_id UUID;

UPDATE sec.sec_permission       SET tenant_id = r.tenant_id FROM sec.sec_role r WHERE r.id = sec_permission.role_id      AND sec_permission.tenant_id IS NULL;
UPDATE sec.sec_field_permission SET tenant_id = r.tenant_id FROM sec.sec_role r WHERE r.id = sec_field_permission.role_id AND sec_field_permission.tenant_id IS NULL;
UPDATE sec.sec_role_assignment  SET tenant_id = r.tenant_id FROM sec.sec_role r WHERE r.id = sec_role_assignment.role_id  AND sec_role_assignment.tenant_id IS NULL;

-- (2) trigger: auto-populate tenant_id from the role on every write
CREATE OR REPLACE FUNCTION mda.sec_tenant_from_role() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    SELECT r.tenant_id INTO NEW.tenant_id FROM sec.sec_role r WHERE r.id = NEW.role_id;
    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS sec_permission_tenant       ON sec.sec_permission;
CREATE TRIGGER sec_permission_tenant       BEFORE INSERT OR UPDATE ON sec.sec_permission
    FOR EACH ROW EXECUTE FUNCTION mda.sec_tenant_from_role();
DROP TRIGGER IF EXISTS sec_field_permission_tenant ON sec.sec_field_permission;
CREATE TRIGGER sec_field_permission_tenant BEFORE INSERT OR UPDATE ON sec.sec_field_permission
    FOR EACH ROW EXECUTE FUNCTION mda.sec_tenant_from_role();
DROP TRIGGER IF EXISTS sec_role_assignment_tenant  ON sec.sec_role_assignment;
CREATE TRIGGER sec_role_assignment_tenant BEFORE INSERT OR UPDATE ON sec.sec_role_assignment
    FOR EACH ROW EXECUTE FUNCTION mda.sec_tenant_from_role();

-- (3) ENABLE + FORCE + tenant_isolation policy on every remaining sec.* table
--     except sec_tenant (the public slug registry login resolves pre-auth).
DO $$
DECLARE t record;
BEGIN
    FOR t IN SELECT tablename FROM pg_tables
              WHERE schemaname='sec'
                AND tablename NOT IN ('sec_tenant')
                AND tablename NOT IN (
                    SELECT relname FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
                     WHERE n.nspname='sec' AND c.relrowsecurity  -- already gated in earlier migrations
                )
    LOOP
        EXECUTE format('ALTER TABLE sec.%I ENABLE ROW LEVEL SECURITY', t.tablename);
        EXECUTE format('ALTER TABLE sec.%I FORCE ROW LEVEL SECURITY',  t.tablename);
        EXECUTE format(
            'DROP POLICY IF EXISTS tenant_isolation ON sec.%I;
             CREATE POLICY tenant_isolation ON sec.%I
             USING (tenant_id = NULLIF(current_setting(''app.tenant_id'', true), '''')::uuid)
             WITH CHECK (tenant_id = NULLIF(current_setting(''app.tenant_id'', true), '''')::uuid)',
            t.tablename, t.tablename);
    END LOOP;
END
$$;
