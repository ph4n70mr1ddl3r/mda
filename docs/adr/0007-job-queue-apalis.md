# ADR-0007: Job queue — apalis (Postgres-backed)

- **Status:** Accepted
- **Date:** 2025-07-16
- **Resolves:** REVIEW.md U7 (job-queue choice)
- **Detail:** PLAN.md §3, §5.9.4

## Context
The outbox-drain worker and scheduled jobs (workflow timers, scheduled reports, ETL schedules, two-phase purge) need a persistent, retry-capable job framework. `apalis` and `tokio-cron-scheduler` have very different reliability semantics; leaving the choice ambiguous blocks implementation.

## Decision
Adopt **apalis** with **Postgres storage** — persistent, with retries, cron, delayed jobs, and middleware. It drives both scheduled jobs and the outbox-drain worker (`FOR UPDATE SKIP LOCKED` on `sys_outbox`). Redis remains for cache, presence, and pub/sub (ADR-0004), not for job durability.

## Consequences
- **(+)** One job framework; durability co-located with the outbox in a single database.
- **(+)** Retries, DLQ, and cron available out of the box.
- **(−)** Job state lives in Postgres (acceptable — it is already the system of record).
