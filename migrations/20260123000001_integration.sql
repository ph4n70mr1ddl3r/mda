-- Integration architecture (PLAN §5.22) — the hub model. Flows materialize
-- external data into the platform's canonical biz.* entities (no stateless
-- pass-through), so the hub applies AuthZ, audit, rules, and transformation
-- between systems. This is generic data-integration mechanics (no vendor/business
-- noun — principle 8).
--
-- Core ships the universal HTTP transport + a pluggable Format/Auth boundary;
-- niche formats/vendor protocols are extension connectors (§5.6/§5.22.6).

-- int.connector: a typed adapter. transport ∈ http (core); extension transports
-- (db/file/mq/graphql/soap + EDI/IDoc/AS2) are add-ons. auth references a
-- sys_secret (§5.20).
CREATE TABLE IF NOT EXISTS int.connector (
    id          UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   UUID        NOT NULL,
    name        TEXT        NOT NULL,            -- also the correlation "system" name
    transport   TEXT        NOT NULL DEFAULT 'http',
    base_url    TEXT        NOT NULL,
    auth        JSONB       NOT NULL DEFAULT '{"kind":"none"}'::jsonb,  -- {kind, secret_ref, ...}
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, name)
);

-- int.flow: an inbound (external→biz) or outbound (biz→external) pipeline.
-- `entity` is the canonical biz entity materialized (hub model). `mapping` binds
-- external fields → biz fields (dotted paths / expression-engine transforms).
-- `external_key_field` names the external payload field used for correlation
-- (int_external_id). `conflict_policy` reconciles cross-system updates.
CREATE TABLE IF NOT EXISTS int.flow (
    id                 UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id          UUID        NOT NULL,
    name               TEXT        NOT NULL,
    direction          TEXT        NOT NULL,            -- inbound | outbound
    entity             TEXT        NOT NULL,            -- canonical biz entity
    connector_id       UUID        REFERENCES int.connector(id) ON DELETE SET NULL,
    webhook_id         UUID,                             -- inbound trigger (§5.21 receiver)
    endpoint_path      TEXT,                             -- e.g. /api/v1/customers (method defaults by direction)
    mapping            JSONB       NOT NULL DEFAULT '{}'::jsonb,
    external_key_field TEXT        NOT NULL DEFAULT 'external_id',
    conflict_policy    TEXT        NOT NULL DEFAULT 'last_write_wins',
    system             TEXT,                             -- correlation system name (defaults to connector name)
    active             BOOLEAN     NOT NULL DEFAULT TRUE,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, name)
);

-- int.flow_step: ordered transform steps applied to the mapped record. Each step
-- may run a bounded expression-engine transform (§5.2) — value translation
-- (int.value_map), conditional, enrichment. Debatching/batching are follow-ups.
CREATE TABLE IF NOT EXISTS int.flow_step (
    id          UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    flow_id     UUID        NOT NULL REFERENCES int.flow(id) ON DELETE CASCADE,
    seq         INTEGER     NOT NULL,
    kind        TEXT        NOT NULL,            -- transform | value_map | filter
    config      JSONB       NOT NULL DEFAULT '{}'::jsonb,
    UNIQUE (flow_id, seq)
);

-- int.value_map: code-set translation tables (status codes, etc.) — data, not
-- code. entries: { "<external>": "<internal>" }.
CREATE TABLE IF NOT EXISTS int.value_map (
    id          UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   UUID        NOT NULL,
    name        TEXT        NOT NULL,
    entries     JSONB       NOT NULL DEFAULT '{}'::jsonb,
    UNIQUE (tenant_id, name)
);

-- RLS on the int.* definition tables (the generic meta-RLS pass ran before
-- these; gate explicitly).
DO $$
DECLARE t record;
BEGIN
    FOR t IN SELECT tablename FROM pg_tables WHERE schemaname='int' LOOP
        EXECUTE format('ALTER TABLE int.%I ENABLE ROW LEVEL SECURITY', t.tablename);
        EXECUTE format('ALTER TABLE int.%I FORCE ROW LEVEL SECURITY',  t.tablename);
        EXECUTE format(
            'DROP POLICY IF EXISTS tenant_isolation ON int.%I;
             CREATE POLICY tenant_isolation ON int.%I
             USING (tenant_id = NULLIF(current_setting(''app.tenant_id'', true), '''')::uuid)
             WITH CHECK (tenant_id = NULLIF(current_setting(''app.tenant_id'', true), '''')::uuid)',
            t.tablename, t.tablename);
    END LOOP;
END
$$;

-- int_external_id: the correlation registry (§5.22.3) — operational, runtime-
-- written by the flow runner (a background worker, like sys_outbox), so it lives
-- in public and is app-layer tenant-filtered (no RLS). Drives upsert-by-external-
-- key, idempotent re-delivery, and cross-path dedup.
CREATE TABLE IF NOT EXISTS int_external_id (
    id            UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id     UUID        NOT NULL,
    entity        TEXT        NOT NULL,
    record_id     UUID        NOT NULL,
    system        TEXT        NOT NULL,
    external_key  TEXT        NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, entity, system, external_key)
);
CREATE INDEX IF NOT EXISTS int_external_id_record_idx
    ON int_external_id (tenant_id, entity, record_id);

-- Per-flow run history (observability, §14 "per-flow run history"). Resumable
-- flows checkpoint here; failures surface for retry/DLQ.
CREATE TABLE IF NOT EXISTS sys_integration_run (
    id            BIGSERIAL   PRIMARY KEY,
    tenant_id     UUID        NOT NULL,
    flow_id       UUID        NOT NULL,
    direction     TEXT        NOT NULL,
    status        TEXT        NOT NULL,            -- ok | failed
    records       INTEGER     NOT NULL DEFAULT 0,
    external_key  TEXT,
    error         TEXT,
    started_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    finished_at   TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS sys_integration_run_flow_idx
    ON sys_integration_run (tenant_id, flow_id, started_at DESC);
