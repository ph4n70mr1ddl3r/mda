-- Phase 0: meta schema skeleton (PLAN §4 / §4.8).
--
-- The meta model *describes* the application. Per ADR-0008 it is FIXED (static
-- Rust + SQL), edited by dedicated Studio handlers — not first-class runtime
-- entities. `biz.*` (business data) tables are generated at publish time, not
-- here. `sec_*` / `sys_*` / `int_*` arrive in their respective phases.
--
-- Every table carries `tenant_id` now (PLAN §5.4); the FK to a real tenant
-- table + Row-Level Security land in Phase 3 (Security & Auth).

CREATE SCHEMA IF NOT EXISTS meta;

-- ===== Entity model (§4.1) =====

-- a logical grouping (e.g. "CRM", "HR")
CREATE TABLE IF NOT EXISTS meta.md_module (
    id          UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   UUID        NOT NULL,
    name        TEXT        NOT NULL,
    label       TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, name)
);

-- a business object definition
CREATE TABLE IF NOT EXISTS meta.md_entity (
    id           UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id    UUID        NOT NULL,
    module_id    UUID        REFERENCES meta.md_module(id),
    table_name   TEXT        NOT NULL,                       -- biz.<table_name>
    name         TEXT        NOT NULL,                       -- programmatic name
    label        TEXT,
    description  TEXT,
    status       TEXT        NOT NULL DEFAULT 'active',      -- active | retired (§5.8)
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, name)
);
CREATE INDEX IF NOT EXISTS md_entity_tenant_idx ON meta.md_entity (tenant_id);

-- attribute definitions (§5.6 type registry)
CREATE TABLE IF NOT EXISTS meta.md_field (
    id            UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id     UUID        NOT NULL,
    entity_id     UUID        NOT NULL REFERENCES meta.md_entity(id) ON DELETE CASCADE,
    name          TEXT        NOT NULL,
    label         TEXT,
    field_type    TEXT        NOT NULL,   -- string|text|integer|decimal|money|bool|date|datetime|enum|reference|json|auto_number
    required      BOOLEAN     NOT NULL DEFAULT FALSE,
    is_unique     BOOLEAN     NOT NULL DEFAULT FALSE,
    is_indexed    BOOLEAN     NOT NULL DEFAULT FALSE,
    default_expr  JSONB,                   -- default value or DSL expression (§5.2)
    config        JSONB       NOT NULL DEFAULT '{}'::jsonb, -- type-specific (precision/scale, enum values, …)
    status        TEXT        NOT NULL DEFAULT 'active',
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, entity_id, name)
);
CREATE INDEX IF NOT EXISTS md_field_entity_idx ON meta.md_field (entity_id);

-- references between entities (§5.7)
CREATE TABLE IF NOT EXISTS meta.md_relationship (
    id                  UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id           UUID        NOT NULL,
    source_entity_id    UUID        NOT NULL REFERENCES meta.md_entity(id) ON DELETE CASCADE,
    source_field_name   TEXT        NOT NULL,           -- hoisted column, e.g. ref_customer_id
    target_entity_id    UUID        NOT NULL REFERENCES meta.md_entity(id),
    cardinality         TEXT        NOT NULL,           -- one_to_many | many_to_one | many_to_many
    strength            TEXT        NOT NULL,           -- master_detail | lookup
    on_delete           TEXT,                           -- restrict | set_null | cascade (lookup only)
    required            BOOLEAN     NOT NULL DEFAULT FALSE,
    reference_qualifier JSONB,                           -- optional DSL predicate
    rollup_summary      JSONB,                           -- optional aggregate spec (ADR-0017)
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, source_entity_id, source_field_name)
);

-- ===== Lifecycle tables (§4.8) =====

-- per-tenant pointer to the live model
CREATE TABLE IF NOT EXISTS meta.md_active_version (
    tenant_id    UUID        PRIMARY KEY,
    version      BIGINT      NOT NULL DEFAULT 0,
    snapshot_id  UUID,
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- editable draft model (JSONB); one `publishing` draft per tenant (ADR-0011)
CREATE TABLE IF NOT EXISTS meta.md_draft (
    id                 UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id          UUID        NOT NULL,
    name               TEXT        NOT NULL,
    parent_snapshot_id UUID,
    model              JSONB       NOT NULL DEFAULT '{}'::jsonb,
    status             TEXT        NOT NULL DEFAULT 'draft', -- draft|validating|publishing|published|failed
    editor_user_id     UUID,
    version_etag       UUID        NOT NULL DEFAULT gen_random_uuid(),
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS md_draft_tenant_idx ON meta.md_draft (tenant_id);
CREATE UNIQUE INDEX IF NOT EXISTS md_draft_one_publishing_idx
    ON meta.md_draft (tenant_id) WHERE status = 'publishing';

-- immutable snapshot archive
CREATE TABLE IF NOT EXISTS meta.md_snapshot (
    id          UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   UUID        NOT NULL,
    version     BIGINT      NOT NULL,
    model       JSONB       NOT NULL,
    manifest    JSONB,                          -- change summary
    created_by  UUID,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS md_snapshot_tenant_version_idx
    ON meta.md_snapshot (tenant_id, version DESC);

-- publish execution log (resume / revert, ADR-0011)
CREATE TABLE IF NOT EXISTS meta.md_migration_log (
    id            UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id     UUID        NOT NULL,
    draft_id      UUID        NOT NULL REFERENCES meta.md_draft(id) ON DELETE CASCADE,
    op            TEXT        NOT NULL,
    status        TEXT        NOT NULL,
    last_id       UUID,                           -- checkpoint cursor
    rows_affected BIGINT      NOT NULL DEFAULT 0,
    started_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    finished_at   TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS md_migration_log_draft_idx ON meta.md_migration_log (draft_id);

-- pending two-phase deletes (§5.8)
CREATE TABLE IF NOT EXISTS meta.md_retirement (
    id           UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id    UUID        NOT NULL,
    kind         TEXT        NOT NULL,           -- field | entity | relationship
    target_id    UUID        NOT NULL,
    retired_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    purge_after  TIMESTAMPTZ NOT NULL,
    status       TEXT        NOT NULL DEFAULT 'retired'   -- retired | purged
);
CREATE INDEX IF NOT EXISTS md_retirement_purge_idx ON meta.md_retirement (status, purge_after);

-- Bootstrap an active-version pointer for a placeholder tenant so the system
-- has a starting point. Real tenant provisioning is deferred (PLAN §5.4).
INSERT INTO meta.md_active_version (tenant_id, version)
VALUES ('00000000-0000-0000-0000-000000000000', 0)
ON CONFLICT (tenant_id) DO NOTHING;
