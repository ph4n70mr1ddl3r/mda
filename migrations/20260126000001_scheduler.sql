-- Scheduled-job management (PLAN §14): modeler-defined schedules with explicit
-- next-run / last-run / last-status / failure state, plus a per-run history.
-- Closes the remaining §14 scheduled-job gap — a generic, cron-driven scheduler
-- that fires due jobs and records each run for the observability surface.
--
-- Job dispatch is pluggable by `kind`. Phase 1 ships `report` (run a saved report
-- under its running user and record the row count) and `integration` (trigger an
-- int.flow). `rule`/`purge`/`custom` kinds follow the same shape.
--
-- These tables live in `public` (qualified on creation, not subject to the
-- `"$user"` search_path quirk) and are app-layer tenant-filtered by `tenant_id`,
-- matching sys_outbox / sys_audit_log. RLS is intentionally not enabled: the
-- scheduler worker claims due rows across tenants (like the outbox drain) with
-- FOR UPDATE SKIP LOCKED, and the REST surface enforces tenant scoping.

CREATE TABLE IF NOT EXISTS sys_schedule (
    id              UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id       UUID        NOT NULL,
    name            TEXT        NOT NULL,
    kind            TEXT        NOT NULL,            -- report | integration | rule | custom
    target_id       UUID        NOT NULL,            -- the scheduled object (report/flow id)
    cron            TEXT        NOT NULL,            -- 6-field cron (sec min hour dom month dow)
    enabled         BOOLEAN     NOT NULL DEFAULT TRUE,
    running_user_id UUID,                            -- AuthZ context captured at run time
    next_run        TIMESTAMPTZ,                     -- NULL until first armed by the API
    last_run        TIMESTAMPTZ,
    last_status     TEXT,                             -- ok | failed | running
    last_error      TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, name)
);
CREATE INDEX IF NOT EXISTS sys_schedule_due_idx
    ON sys_schedule (next_run) WHERE enabled AND next_run IS NOT NULL;

-- Per-run history (the §14 "next-run/last-run/failure state" observability feed).
CREATE TABLE IF NOT EXISTS sys_schedule_run (
    id           BIGSERIAL   PRIMARY KEY,
    tenant_id    UUID        NOT NULL,
    schedule_id  UUID        NOT NULL REFERENCES sys_schedule(id) ON DELETE CASCADE,
    status       TEXT        NOT NULL,            -- ok | failed
    rows_affected INTEGER    NOT NULL DEFAULT 0,
    error        TEXT,
    started_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    finished_at  TIMESTAMPTZ,
    duration_ms  INTEGER
);
CREATE INDEX IF NOT EXISTS sys_schedule_run_sched_idx
    ON sys_schedule_run (tenant_id, schedule_id, started_at DESC);
