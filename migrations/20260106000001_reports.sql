-- Phase 7: reporting (PLAN §4.4 / §5.17). A report is a structured dataset
-- (base entity + select fields + filters + group_by + order_by + limit) compiled
-- to parameterized SQL over biz.<table>; the engine enforces the runner's
-- object/field/record security by construction.

CREATE TABLE IF NOT EXISTS meta.md_report (
    id          UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   UUID        NOT NULL,
    name        TEXT        NOT NULL,
    dataset     JSONB       NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, name)
);
