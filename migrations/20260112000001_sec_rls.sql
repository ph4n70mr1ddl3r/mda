-- RLS on the sec.* record-access tables (defense-in-depth on top of the
-- app-layer tenant filter). Scope is deliberately narrow — only the sec.*
-- tables that (a) carry tenant_id and (b) are read/written only in request
-- context (never by login or background workers):
--
--   sec_record_share  — the materialized sharing grants (who can read/write what)
--   sec_owd           — org-wide default per entity
--
-- The other sec.* tables are INTENTIONALLY NOT gated here, with specific
-- blockers (a clean fix is a focused follow-up, not a one-liner):
--   sec_user            — login (auth.rs) looks it up by EMAIL with no tenant
--                         context (the JWT is issued from that lookup). Gating it
--                         needs tenant-scoped login or a separate auth role.
--   sec_permission /    — keyed by role_id, NO tenant_id column. A tenant policy
--     sec_field_          needs either a subquery (recursive with sec_role's own
--     permission /          RLS) or denormalising tenant_id onto them.
--     sec_role_assignment
-- Background workers touch only sys_*, so no sec.* exemption is needed.

DO $$
DECLARE t record;
BEGIN
    FOR t IN SELECT tablename FROM pg_tables
              WHERE schemaname='sec' AND tablename IN ('sec_record_share','sec_owd')
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
