-- Two fixes for the inbound webhook receiver's SECURITY DEFINER lookup
-- (`mda.lookup_webhook`, created by migration 20260122000001), which assumed a
-- superuser migration role. Both surface only in restricted environments, so
-- they ship as a follow-up rather than editing an applied migration (sqlx
-- checksums).
--
-- Fix 1: FORCE RLS hid every row from the lookup on non-superuser deployments.
--
--   20260122000001's comment says the function is "owned by the migration role
--   (which bypasses RLS)". That holds only when the migration role is a
--   SUPERUSER: `int.webhook` was left with ENABLE *and FORCE* ROW LEVEL
--   SECURITY, and under FORCE the table OWNER is subject to the policies too.
--   The lookup runs with no `app.tenant_id` set, so on any deployment whose
--   migrations run as a non-superuser owner — every managed Postgres (RDS,
--   Cloud SQL), which never grants superuser — it saw zero rows and EVERY
--   inbound webhook was rejected with 404.
--
--   Drop FORCE on `int.webhook` only. Tenant isolation for serving roles is
--   unaffected: RLS stays ENABLED and every non-owner role (`mda_app`
--   included, via 20260135000001's DML grants on `int`) remains fully subject
--   to the tenant_isolation policy. Only the owner context — exactly the one
--   SECURITY DEFINER lookup — crosses tenants, which is its documented,
--   deliberately single bypass.
ALTER TABLE int.webhook NO FORCE ROW LEVEL SECURITY;

-- Fix 2: the EXECUTE grant on the lookup was unconditional in 20260122000001.
--
--   Migration 20260111000001 deliberately treats `mda_app` as OPTIONAL (it is
--   skipped where the migrating role lacks CREATEROLE), and every later
--   migration guards its grants with `IF EXISTS` — except 20260122000001's
--   bare `GRANT ... TO mda_app`, which hard-failed the whole migration chain
--   wherever the role legitimately does not exist. Re-issue it guarded and
--   idempotently (a no-op where the original grant already succeeded).
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'mda_app') THEN
        GRANT EXECUTE ON FUNCTION mda.lookup_webhook(uuid) TO mda_app;
    END IF;
END
$$;
