-- Event & webhook contract (PLAN §5.21) + inbound verification (§14).
--
-- Outbound: a versioned, HMAC-signed JSON envelope delivered to webhook
-- subscribers, via the transactional outbox (at-least-once, DLQ). The contract
-- is structural; event types/payloads are metadata/extension-defined. event_id
-- is the idempotency key; schema_version lets consumers evolve without breakage.
--
-- Inbound (§14): shared-secret / signature / replay protection for the inbound
-- receiver — mirrors the outbound contract so an external system can POST events
-- in with the same signing scheme.

CREATE SCHEMA IF NOT EXISTS int;

-- A webhook subscription (definition). event_types '{}' or {'*'} = all;
-- entity_filter NULL = all entities. secret_ref names a sys_secret holding the
-- HMAC key (§5.20). The relay applies AuthZ: a subscriber receives events only
-- for records/fields its principal may see (per-client filtering, §5.10.6) —
-- owner-based for v1; full ABAC subscription filter is a follow-up.
CREATE TABLE IF NOT EXISTS int.webhook (
    id            UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id     UUID        NOT NULL,
    name          TEXT        NOT NULL,
    url           TEXT        NOT NULL,
    event_types   TEXT[]      NOT NULL DEFAULT '{}',   -- '{}' | {'*'} | {'record.created', ...}
    entity_filter TEXT,                                 -- NULL = all entities
    secret_ref    TEXT        NOT NULL,                 -- sys_secret name (HMAC key)
    active        BOOLEAN     NOT NULL DEFAULT TRUE,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, name)
);
ALTER TABLE int.webhook ENABLE ROW LEVEL SECURITY;
ALTER TABLE int.webhook FORCE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON int.webhook;
CREATE POLICY tenant_isolation ON int.webhook
    USING (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid);

-- Per-delivery attempt log (idempotent on (webhook, event_id)).
CREATE TABLE IF NOT EXISTS sys_webhook_delivery (
    id            BIGSERIAL   PRIMARY KEY,
    tenant_id     UUID        NOT NULL,
    webhook_id    UUID        NOT NULL,
    event_id      TEXT        NOT NULL,          -- envelope event_id (idempotency key)
    event_type    TEXT,
    entity        TEXT,
    record_id     UUID,
    url           TEXT        NOT NULL,
    status        TEXT        NOT NULL DEFAULT 'pending',  -- pending | delivered | failed
    response_code INTEGER,
    attempts      INTEGER     NOT NULL DEFAULT 0,
    last_error    TEXT,
    delivered_at  TIMESTAMPTZ,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (webhook_id, event_id)
);
CREATE INDEX IF NOT EXISTS sys_webhook_delivery_pending_idx
    ON sys_webhook_delivery (tenant_id, status, created_at);

-- Inbound webhook log (§14): received + verified payloads, awaiting an
-- integration flow (§5.22) to consume them. replay-protected by the timestamp
-- in the signature; duplicates are deduped on (webhook_id, event_id).
CREATE TABLE IF NOT EXISTS sys_inbound_webhook (
    id            BIGSERIAL   PRIMARY KEY,
    tenant_id     UUID        NOT NULL,
    webhook_id    UUID        NOT NULL,
    event_id      TEXT,                            -- caller-supplied idempotency key (X-MDA-Event-Id)
    event_type    TEXT,
    payload       JSONB       NOT NULL DEFAULT '{}'::jsonb,
    received_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    processed     BOOLEAN     NOT NULL DEFAULT FALSE,
    UNIQUE (webhook_id, event_id)
);
CREATE INDEX IF NOT EXISTS sys_inbound_webhook_unprocessed_idx
    ON sys_inbound_webhook (tenant_id, processed, received_at);

-- The inbound receiver has no auth context (it IS the edge entry point for an
-- external system), so it cannot set the tenant GUC before resolving which
-- tenant a webhook belongs to. A SECURITY DEFINER function owned by the
-- migration role (which bypasses RLS) resolves (tenant, secret, url, active) by
-- id — the ONLY RLS bypass for int.webhook, scoped to this one lookup.
CREATE OR REPLACE FUNCTION mda.lookup_webhook(p_id uuid)
RETURNS TABLE (tenant_id uuid, secret_ref text, url text, active boolean)
LANGUAGE sql SECURITY DEFINER SET search_path = int, public AS $$
    SELECT tenant_id, secret_ref, url, active FROM int.webhook WHERE id = p_id;
$$;
GRANT EXECUTE ON FUNCTION mda.lookup_webhook(uuid) TO mda_app;
