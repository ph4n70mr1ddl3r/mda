-- Phase 5/6 follow-up: in-app notifications (PLAN §4.7 / §5.18), fed by the
-- outbox drain worker.

CREATE TABLE IF NOT EXISTS sys_notification (
    id          UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   UUID        NOT NULL,
    user_id     UUID        NOT NULL,
    type        TEXT        NOT NULL,
    entity      TEXT,
    record_id   UUID,
    payload     JSONB       NOT NULL DEFAULT '{}'::jsonb,
    read_at     TIMESTAMPTZ,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS sys_notification_user_idx
    ON sys_notification (tenant_id, user_id, read_at, created_at DESC);
