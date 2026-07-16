-- Phase 6: advanced record security (PLAN §5.11.3 / ADR-0013).
-- sec_record_share is the MATERIALIZED visibility table (manual shares now;
-- criteria-rule-derived shares + epoch-gated recompute follow). Enforcement is a
-- query-rewrite predicate (owner OR public OR shared-with-me), never post-filter.

CREATE TABLE IF NOT EXISTS sec.sec_share_rule (
    id           UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id    UUID        NOT NULL,
    entity       TEXT        NOT NULL,
    condition    JSONB       NOT NULL DEFAULT '{"op":"Lit","value":true}'::jsonb,
    principal_id UUID        NOT NULL,   -- user/team the rule shares WITH
    access       TEXT        NOT NULL,   -- read | write
    epoch        BIGINT      NOT NULL DEFAULT 1,
    active       BOOLEAN     NOT NULL DEFAULT TRUE,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS sec_share_rule_entity_idx ON sec.sec_share_rule (tenant_id, entity, active);

-- materialized shares: rule_id NULL => manual share; epoch NULL => always honored.
CREATE TABLE IF NOT EXISTS sec.sec_record_share (
    id           UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id    UUID        NOT NULL,
    entity       TEXT        NOT NULL,
    record_id    UUID        NOT NULL,
    principal_id UUID        NOT NULL,
    access       TEXT        NOT NULL,   -- read | write
    rule_id      UUID        REFERENCES sec.sec_share_rule(id) ON DELETE CASCADE,
    epoch        BIGINT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, record_id, principal_id)
);
CREATE INDEX IF NOT EXISTS sec_record_share_principal_idx
    ON sec.sec_record_share (tenant_id, principal_id, record_id);

-- optional role hierarchy ("see records below me"); hierarchy-derived shares
-- arrive with the full recompute machinery.
CREATE TABLE IF NOT EXISTS sec.sec_role_hierarchy (
    tenant_id  UUID        NOT NULL,
    role_id    UUID        NOT NULL REFERENCES sec.sec_role(id) ON DELETE CASCADE,
    parent_id  UUID        NOT NULL REFERENCES sec.sec_role(id) ON DELETE CASCADE,
    PRIMARY KEY (tenant_id, role_id, parent_id)
);
