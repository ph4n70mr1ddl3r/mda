-- Outbox-driven notification deliveries must be idempotent per
-- (outbox row, recipient) so an at-least-once replay — a worker crash
-- between delivering and stamping the row, or a retry after a row-level
-- failure — is a no-op instead of a duplicate email.
--
-- sys_notification derives its UUID primary key deterministically from the
-- outbox row id (uuid v5) and needs no schema change. sys_message has a
-- BIGSERIAL pk, so it carries the dedupe key as a nullable column with a
-- partial unique index (NULL = not outbox-driven; those rows are exempt).

ALTER TABLE sys_message ADD COLUMN IF NOT EXISTS outbox_id UUID;

CREATE UNIQUE INDEX IF NOT EXISTS sys_message_outbox_dedupe_idx
    ON sys_message (outbox_id, user_id)
    WHERE outbox_id IS NOT NULL;

COMMENT ON COLUMN sys_message.outbox_id IS
    'sys_outbox row that produced this message — dedupe key for at-least-once replays (NULL for rows written outside the outbox)';
