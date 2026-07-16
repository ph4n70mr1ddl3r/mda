# ADR-0003: Concurrency & transactional outbox

- **Status:** Accepted
- **Date:** 2025-07-16
- **Resolves:** REVIEW.md C4
- **Detail:** PLAN.md §5.9

## Context
Multi-step writes (record mutation + rules + workflow transition + notifications) must be atomic, and external side-effects (webhook/email) must be delivered reliably without coupling request latency to external systems or risking the dual-write problem (commit to DB, then fail to publish).

## Decision
- **Optimistic concurrency** via a `version` column (conditional `UPDATE … WHERE version = $expected` → 409 on conflict); soft checkout (`sys_lock`) is advisory UX only.
- **Transactional outbox**: the data change and a `sys_outbox` row are written in one transaction; workers drain via `FOR UPDATE SKIP LOCKED` with backoff + DLQ; delivery is at-least-once, so consumers are idempotent.
- **Rule/workflow split**: data-affecting logic is synchronous & atomic in the write transaction; notifications are eventual via the outbox. Workflow timers serialize via `FOR UPDATE`.

## Consequences
- **(+)** Data integrity is trustworthy; side-effects are durable and decoupled from request latency.
- **(+)** A clear "trustworthy vs eventual" rule.
- **(−)** All async consumers must be idempotent; worker processes add an operational component.
