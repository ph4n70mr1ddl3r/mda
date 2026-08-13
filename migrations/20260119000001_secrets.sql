-- Secrets management (PLAN §5.20). Connector/channel/integration credentials
-- are per-tenant and NEVER stored as plaintext in metadata. `sys_secret` holds
-- only a REFERENCE (name, kind, ref); the secret VALUE lives in an external
-- `SecretStore` (LocalSecretStore in dev — env / JSON file; cloud KMS / Vault
-- in prod, follow-up impls). Values are resolved server-side only, under the
-- connector's authz, never returned by any API, never logged, and never
-- serialized into events/audit/outbox payloads.
--
-- Every value resolution is audited (`sys_secret_audit`): who/when resolved
-- which secret and for what purpose. References are tenant-scoped.

CREATE TABLE IF NOT EXISTS sys_secret (
    id          UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   UUID        NOT NULL,
    name        TEXT        NOT NULL,            -- modeler-facing key, e.g. "smtp_password"
    kind        TEXT        NOT NULL DEFAULT 'opaque',  -- opaque | oauth_refresh | api_key | ...
    ref         TEXT        NOT NULL,            -- store-specific key (env var / file key / KMS id)
    rotated_at  TIMESTAMPTZ,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, name)
);
CREATE INDEX IF NOT EXISTS sys_secret_tenant_idx ON sys_secret (tenant_id, name);

-- Audit of every value resolution (§5.20 "rotation & audit"). `name` is
-- denormalised so a row remains readable after the secret ref is deleted.
CREATE TABLE IF NOT EXISTS sys_secret_audit (
    id          BIGSERIAL   PRIMARY KEY,
    tenant_id   UUID        NOT NULL,
    secret_id   UUID        NOT NULL,
    name        TEXT        NOT NULL,
    resolved_by UUID,                            -- the actor (user) or NULL for a background worker
    purpose     TEXT        NOT NULL DEFAULT 'default',  -- e.g. "webhook.sign" / "connector.auth"
    resolved_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS sys_secret_audit_secret_idx
    ON sys_secret_audit (tenant_id, secret_id, resolved_at DESC);
