-- Notifications & messaging (PLAN §5.18) — the full platform subsystem built on
-- sys_notification + the real-time channel + the transactional outbox. The
-- engine knows no notification *content*: types + templates are metadata; this
-- is the generic delivery machinery.
--
-- (1) md_notification_type — a type is metadata (authored in Studio): an opaque
--     key (the modeler's name; the engine treats it opaquely), default channels,
--     a link to a template (§5.19), and whether it is digestible.
-- (2) sys_notification_preference — per-user overrides (mute a type / opt out of
--     a channel). Defaults come from the type; honored at FAN-OUT time, so a
--     muted type is never produced (not merely hidden).
-- (3) sys_message — a delivered email/message log (every channel except in-app
--     is an async side-effect via the outbox; SMTP send is a follow-up, but the
--     delivered message is recorded here for audit + tests).

CREATE TABLE IF NOT EXISTS meta.md_notification_type (
    id               UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id        UUID        NOT NULL,
    key              TEXT        NOT NULL,            -- opaque to the engine (e.g. invoice.overdue)
    label            TEXT        NOT NULL,
    default_channels TEXT[]      NOT NULL DEFAULT '{in_app}',  -- in_app | email | webhook
    template_name    TEXT,                             -- link to meta.md_template (§5.19)
    digestible       BOOLEAN     NOT NULL DEFAULT FALSE,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, key)
);
ALTER TABLE meta.md_notification_type ENABLE ROW LEVEL SECURITY;
ALTER TABLE meta.md_notification_type FORCE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON meta.md_notification_type;
CREATE POLICY tenant_isolation ON meta.md_notification_type
    USING (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid)
    WITH CHECK (tenant_id = NULLIF(current_setting('app.tenant_id', true), '')::uuid);

-- Per-user channel/type preferences. (opted_in = false ⇒ muted/opted-out.)
CREATE TABLE IF NOT EXISTS sys_notification_preference (
    id          UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   UUID        NOT NULL,
    user_id     UUID        NOT NULL,
    type_key    TEXT        NOT NULL,
    channel     TEXT        NOT NULL,            -- in_app | email | webhook
    opted_in    BOOLEAN     NOT NULL,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, user_id, type_key, channel)
);
CREATE INDEX IF NOT EXISTS sys_notification_pref_user_idx
    ON sys_notification_preference (tenant_id, user_id, type_key);

-- A delivered email/message log. `to_addr` is resolved from sec_user.email at
-- fan-out; SMTP transport is a follow-up (the message is recorded here).
CREATE TABLE IF NOT EXISTS sys_message (
    id           BIGSERIAL   PRIMARY KEY,
    tenant_id    UUID        NOT NULL,
    user_id      UUID,                            -- recipient (NULL for external addrs)
    to_addr      TEXT,
    type_key     TEXT        NOT NULL,
    subject      TEXT,
    body         TEXT,
    content_type TEXT        NOT NULL DEFAULT 'text/plain',
    record_id    UUID,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS sys_message_user_idx ON sys_message (tenant_id, user_id, created_at DESC);

-- Digest support: when a digestible type's unread notifications are rolled into
-- a single summary, the originals are marked digested_at (so they no longer
-- surface individually but remain in the timeline).
ALTER TABLE sys_notification ADD COLUMN IF NOT EXISTS digested_at TIMESTAMPTZ;
