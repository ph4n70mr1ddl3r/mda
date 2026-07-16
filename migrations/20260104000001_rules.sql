-- Phase 4: business rules (PLAN §4.3 / §5.9). Phase-4 supports set-field
-- actions firing synchronously in the write transaction (conditions + values
-- use the bounded expression DSL). More action kinds + async outbox side-effects
-- arrive later.

CREATE TABLE IF NOT EXISTS meta.md_rule (
    id           UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id    UUID        NOT NULL,
    entity       TEXT        NOT NULL,
    event        TEXT        NOT NULL,   -- after_create | after_update | before_create | before_update
    condition    JSONB       NOT NULL DEFAULT '{"op":"Lit","value":true}'::jsonb,
    action_type  TEXT        NOT NULL,   -- set_field
    action_field TEXT,
    action_value JSONB,
    active       BOOLEAN     NOT NULL DEFAULT TRUE,
    priority     INTEGER     NOT NULL DEFAULT 100,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS md_rule_entity_idx ON meta.md_rule (tenant_id, entity, active);
