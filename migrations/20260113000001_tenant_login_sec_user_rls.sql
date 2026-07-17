-- Option 1: tenant-scoped login unblocks sec_user RLS.
--
-- (1) A PUBLIC tenant registry (slug -> tenant_id). Login resolves the tenant
--     from the request (slug, or UUID) BEFORE it has any tenant context, so
--     sec_tenant carries NO RLS — slugs are public identifiers (like a
--     subdomain). The app then sets `app.tenant_id` and the sec_user lookup is
--     tenant-scoped (RLS-enforced).
--
-- (2) sec_user RLS. Previously exempt because login looked it up by email with
--     no tenant; now login is tenant-scoped, so sec_user can be gated like the
--     other tenant tables. Every sec_user read/write now runs under the GUC:
--       - login            : GUC set from the resolved tenant
--       - load_identity    : GUC set from the verified JWT's tenant claim
--       - create_share     : GUC set (principal check)
--       - bootstrap        : GUC set (bootstrap tenant) — runs as the owner,
--                            which is a superuser in prod (bypasses) but may be
--                            a non-superuser in dev, so it sets the GUC too.

CREATE TABLE IF NOT EXISTS sec.sec_tenant (
    id          UUID        PRIMARY KEY,
    slug        TEXT        NOT NULL UNIQUE,
    name        TEXT        NOT NULL,
    active      BOOLEAN     NOT NULL DEFAULT TRUE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS sec_tenant_slug_idx ON sec.sec_tenant (slug);

-- Backfill the bootstrap (all-zeros) tenant with a loginable slug.
INSERT INTO sec.sec_tenant (id, slug, name)
VALUES ('00000000-0000-0000-0000-000000000000', 'default', 'Default')
ON CONFLICT (id) DO NOTHING;

ALTER TABLE sec.sec_user ENABLE ROW LEVEL SECURITY;
ALTER TABLE sec.sec_user FORCE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON sec.sec_user;
CREATE POLICY tenant_isolation ON sec.sec_user
  USING (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid)
  WITH CHECK (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid);
