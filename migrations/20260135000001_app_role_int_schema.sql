-- Grant the non-superuser app role (`mda_app`) access to the `int` schema —
-- the Phase 9 integration + webhook surface.
--
-- Root cause: migrations 20260111000001 (RLS) and later granted `mda_app`
-- USAGE + full DML + default privileges for `meta`, `sec` and `public`, but
-- the `int` schema (created later by 20260122000001: int.webhook, int.flow,
-- int.connector, int.external_id, int.integration_run) never received the
-- same treatment. The app serves as `mda_app` in every release/staging
-- deployment, so EVERY webhook-subscription and integration API call failed
-- with "permission denied for schema int" (a 500 on POST /api/webhooks,
-- GET /api/flows, …). Dev hid it: the dev server runs as the owner role.
--
-- Mirrors the 20260111 grant shape for the missing schema, including default
-- privileges so future int.* tables are covered automatically. int.* tables
-- are app-layer tenant-filtered by tenant_id (no RLS by design — the workers
-- claim rows across tenants with FOR UPDATE SKIP LOCKED, like the outbox).
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'mda_app') THEN
        GRANT USAGE ON SCHEMA int TO mda_app;
        GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA int TO mda_app;
        GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA int TO mda_app;
        -- Tenant-extraction helpers used by the RLS-backed workers
        -- (int_tenant_from_flow) and the outbox relay (lookup_webhook).
        GRANT EXECUTE ON ALL FUNCTIONS IN SCHEMA int TO mda_app;
        ALTER DEFAULT PRIVILEGES IN SCHEMA int
            GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO mda_app;
        ALTER DEFAULT PRIVILEGES IN SCHEMA int
            GRANT USAGE, SELECT ON SEQUENCES TO mda_app;
    END IF;
END
$$;
