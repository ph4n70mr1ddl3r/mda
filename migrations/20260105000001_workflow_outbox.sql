-- Phase 5: workflow state machines (PLAN §4.3 / §5.9.5) + the transactional
-- outbox (§5.9.4). The record's current state lives in the core `state` column
-- of biz.<table>; transitions are authored as metadata and executed in the write
-- transaction. The outbox drain worker is a follow-up; the table is in place.

CREATE TABLE IF NOT EXISTS meta.md_workflow (
    id          UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   UUID        NOT NULL,
    entity      TEXT        NOT NULL,
    name        TEXT        NOT NULL,
    active      BOOLEAN     NOT NULL DEFAULT TRUE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, entity, name)
);

CREATE TABLE IF NOT EXISTS meta.md_workflow_state (
    id          UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    workflow_id UUID        NOT NULL REFERENCES meta.md_workflow(id) ON DELETE CASCADE,
    name        TEXT        NOT NULL,
    UNIQUE (workflow_id, name)
);

-- from_state / to_state are state NAMES within the workflow.
-- guard is a DSL expression (§5.2); actions is a JSON array of {field, value(expr)}.
CREATE TABLE IF NOT EXISTS meta.md_workflow_transition (
    id            UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    workflow_id   UUID        NOT NULL REFERENCES meta.md_workflow(id) ON DELETE CASCADE,
    name          TEXT        NOT NULL,
    from_state    TEXT        NOT NULL,
    to_state      TEXT        NOT NULL,
    guard         JSONB       NOT NULL DEFAULT '{"op":"Lit","value":true}'::jsonb,
    actions       JSONB       NOT NULL DEFAULT '[]'::jsonb,
    creates_task  BOOLEAN     NOT NULL DEFAULT FALSE,
    UNIQUE (workflow_id, name)
);

-- user tasks / approvals (PLAN §4.3).
CREATE TABLE IF NOT EXISTS meta.md_workflow_task (
    id            UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id     UUID        NOT NULL,
    workflow_id   UUID        NOT NULL REFERENCES meta.md_workflow(id) ON DELETE CASCADE,
    entity        TEXT        NOT NULL,
    record_id     UUID        NOT NULL,
    transition_id UUID,
    assignee_id   UUID,
    status        TEXT        NOT NULL DEFAULT 'pending',  -- pending | done | cancelled
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at  TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS md_workflow_task_record_idx
    ON meta.md_workflow_task (tenant_id, entity, record_id);

-- transactional outbox: durable pending side-effects (§5.9.4). Drain worker TBD.
CREATE TABLE IF NOT EXISTS sys_outbox (
    id          UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   UUID        NOT NULL,
    kind        TEXT        NOT NULL,        -- workflow.transitioned | notification | webhook | ...
    payload     JSONB       NOT NULL DEFAULT '{}'::jsonb,
    status      TEXT        NOT NULL DEFAULT 'pending',  -- pending | done | failed
    attempts    INTEGER     NOT NULL DEFAULT 0,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    processed_at TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS sys_outbox_pending_idx ON sys_outbox (status, created_at);
