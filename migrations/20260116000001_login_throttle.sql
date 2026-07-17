-- Login throttling (PLAN §3): brute-force / credential-stuffing defence.
--
-- A small, cross-tenant counter table that is deliberately NOT under RLS: keys
-- include non-tenant IP rows (`ip:<addr>`), and account rows are matched by an
-- exact `key` lookup (no tenant scan), so RLS would only get in the way. Because
-- it lives in Postgres it is shared across app instances — the limit holds no
-- matter which replica serves the request. Managed by `mda_security::login_throttle`.
--
--   key              namespace-prefixed identifier:
--                      acct:<tenant_id>:<email>   per-account progressive lockout
--                      ip:<client_ip>             per-IP rate limit
--   fail_count       failures in the current burst
--   first_failed_at  start of the current burst (drives the rolling window)
--   locked_until     when > now(), the key is locked (login → 429)
--   last_attempt_at  bumped every attempt; the cleanup worker prunes on this
--
-- Like the other sys_* tables (sys_event_log, sys_outbox, …) this lives in the
-- default schema with an `sys_` name prefix — there is no separate `sys` schema.

CREATE TABLE IF NOT EXISTS sys_login_throttle (
    key              TEXT        NOT NULL,
    fail_count       INT         NOT NULL DEFAULT 0,
    first_failed_at  TIMESTAMPTZ,
    locked_until     TIMESTAMPTZ,
    last_attempt_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (key)
);

-- Grant the app role full CRUD if it exists (guard: not all deployments create
-- `mda_app`). Targets the table directly so it can't regress like a schema-wide
-- grant would.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'mda_app') THEN
        GRANT SELECT, INSERT, UPDATE, DELETE ON sys_login_throttle TO mda_app;
    END IF;
END
$$;
