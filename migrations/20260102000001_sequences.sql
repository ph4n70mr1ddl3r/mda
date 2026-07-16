-- Phase 2: auto_number sequence table (PLAN §5.6 / §9).
-- Gapless, concurrency-safe per (tenant, entity, field); incremented within the
-- record's write transaction (row-locked via INSERT ... ON CONFLICT DO UPDATE).

CREATE TABLE IF NOT EXISTS meta.md_sequence (
    tenant_id   UUID NOT NULL,
    entity_id   UUID NOT NULL,
    field_name  TEXT NOT NULL,
    next        BIGINT NOT NULL DEFAULT 1,
    PRIMARY KEY (tenant_id, entity_id, field_name)
);
