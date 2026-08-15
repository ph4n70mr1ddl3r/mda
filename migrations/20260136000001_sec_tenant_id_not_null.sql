-- Tighten the tenant_id columns added by 20260114000001_sec_rls_remaining.sql:
-- the backfill populated every row and a BEFORE INSERT/UPDATE trigger keeps
-- them populated, but the columns stayed nullable. The RLS policy
-- `tenant_id = current_setting('app.tenant_id', true)::uuid` treats NULL as
-- non-matching, so a NULL row would fail closed by silently disappearing —
-- make that state unrepresentable instead.
ALTER TABLE sec.sec_permission      ALTER COLUMN tenant_id SET NOT NULL;
ALTER TABLE sec.sec_field_permission ALTER COLUMN tenant_id SET NOT NULL;
ALTER TABLE sec.sec_role_assignment ALTER COLUMN tenant_id SET NOT NULL;
