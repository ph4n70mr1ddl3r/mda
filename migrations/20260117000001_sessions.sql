-- Server-side sessions for revocable refresh tokens (PLAN §3).
--
-- One row per login. The refresh token's `sid` claim names a row here; refresh
-- rotates it (revoke old + insert new) and detects reuse (a refresh for an
-- already-revoked row revokes all of the user's sessions). logout revokes the
-- row named by the access token's `sid`. Access tokens themselves stay stateless
-- (15 m), so revocation lands on the next refresh, at the latest within the TTL.
--
-- Tenant-scoped and RLS-gated like the rest of sec.*, accessed only in request
-- context under the tenant GUC.

CREATE TABLE IF NOT EXISTS sec.sec_session (
    id            UUID        PRIMARY KEY,
    tenant_id     UUID        NOT NULL,
    user_id       UUID        NOT NULL,
    issued_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at    TIMESTAMPTZ NOT NULL,
    last_used_at  TIMESTAMPTZ,
    revoked_at    TIMESTAMPTZ,
    ip            TEXT
);

CREATE INDEX IF NOT EXISTS sec_session_user_idx
    ON sec.sec_session (tenant_id, user_id, revoked_at);

ALTER TABLE sec.sec_session ENABLE ROW LEVEL SECURITY;
ALTER TABLE sec.sec_session FORCE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON sec.sec_session;
CREATE POLICY tenant_isolation ON sec.sec_session
    USING (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid);

DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'mda_app') THEN
        GRANT SELECT, INSERT, UPDATE, DELETE ON sec.sec_session TO mda_app;
    END IF;
END
$$;
