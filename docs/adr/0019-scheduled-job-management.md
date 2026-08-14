# ADR-0019: Scheduled-job management — cron-driven scheduler

- **Status:** Accepted
- **Date:** 2026-01-26
- **Resolves:** PLAN §14 "Scheduled-job management" (was open)
- **Detail:** PLAN §14; implementation `crates/mda-api/src/schedules.rs`

## Context

§14 tracked "scheduled-job management for modeler-defined schedules (next-run /
last-run / failure state)" as an open platform gap. The outbox console
(ADR-0018) surfaced delivery *failure* state, but there was no user-facing
scheduler: no way to define a cron schedule, see when it next fires, or review
its run history. ADR-0007 nominated **apalis** (Postgres-backed) as the job
framework, but the codebase's only existing background worker — the outbox
drain — is hand-rolled against Postgres (`FOR UPDATE SKIP LOCKED`), not apalis.

## Decision

Ship a **generic, cron-driven scheduler** (`sys_schedule` + `sys_schedule_run`)
implemented in the same hand-rolled style as the outbox drain, rather than
introducing apalis mid-stream. A worker claims due rows with
`FOR UPDATE SKIP LOCKED` (multi-instance safe), advances `next_run` *before*
dispatch (so a transient failure never blocks the cadence), runs the job under
its **running user**'s AuthZ (`mda_security::load_identity`), and records each
run (`status` / `rows_affected` / `error`).

- **Cron:** 6-field expressions (`sec min hour dom month dow`, UTC) via the
  `cron` crate.
- **Dispatch by `kind`:** `report` runs a saved report under the running user;
  `custom` is a no-op extensibility hook (and the scheduler test). Additional
  kinds (`integration` pull, scheduled `rule`) reuse the same shape.
- **REST surface:** `GET/POST /api/schedules`, `GET/PATCH/DELETE
  /api/schedules/:id`, `POST /api/schedules/:id/run` (manual trigger, also
  recorded), `GET /api/schedules/:id/runs`.

## Consequences

- **(+)** Closes the §14 gap with a management surface, run history, and a
  working cron scheduler — multi-instance safe, AuthZ-honoring, observable.
- **(+)** Reuses the proven outbox-drain pattern (`FOR UPDATE SKIP LOCKED`,
  drain-then-sleep) and the running-user AuthZ semantics already used by
  `md_report_schedule` (§5.17).
- **(−)** Diverges from ADR-0007's apalis nomination. The divergence is
  deliberate and already precedent (the outbox drain is hand-rolled): one job
  framework was never actually adopted, and a focused hand-rolled scheduler is
  smaller, fully tested, and avoids pulling apalis's full surface for the
  current scope. ADR-0007 should be re-read as "Postgres-backed durability for
  background work" — satisfied by both the outbox drain and this scheduler.
- **(−)** Cron resolution is UTC-only in v1; per-tenant timezones and DST-aware
  scheduling are follow-ups (store a tz on `sys_schedule` and convert at
  dispatch). Only `report`/`custom` dispatch ships now; `integration` pull and
  scheduled `rule` kinds are the obvious next additions.
