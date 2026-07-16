# ADR-0016: Chained workflow transitions — sync-by-default (atomic) vs async (with required failure handling)

- **Status:** Accepted
- **Date:** 2025-07-16
- **Refines:** ADR-0003 (workflow execution model, §5.9.5)
- **Detail:** PLAN.md §5.9.5

## Context

§5.9.5 (under ADR-0003) routes every chained workflow transition through the
outbox: "a transition that should trigger *another* transition emits a domain
event to the outbox rather than chaining synchronously — keeping each transaction
bounded and avoiding cascade locks." Stated as pure upside, it has an unacknowledged
cost: **chained transitions are no longer atomic.** In `Approve → auto-Fulfill`,
Approve commits immediately while Fulfill runs later via the outbox; if Fulfill's
guard fails or an external dependency is down, the record is left *Approved but not
Fulfilled* — a partial completion with no defined compensation. For approval and
finance workflows that is often unacceptable, and the partial state is silent unless
something surfaces it.

The "bounded transaction" rationale is real for *deep/wide* chains and *cross-system*
ones, but it is the wrong default for the common case: an immediate, in-process
dependent transition (Approve → Fulfill where Fulfill is pure data) that *should* be
all-or-nothing. Forcing it async manufactures a partial-completion hazard that
atomicity would have prevented for free.

## Decision

Make chaining a **modeler-declared choice per trigger**, defaulting to **sync**
for in-process chains and requiring **declared failure handling** for async ones —
so atomicity is available where it is achievable, and partial completion is never
silent where it is not.

### Sync chains (default; in-process, atomic)

A transition may chain to the next transition **synchronously within the same
transaction**:

- **All-or-nothing.** If any step's guard fails or errors, the entire chain rolls
  back — Approve and Fulfill commit together or not at all.
- **Bounded by the recursion budget** (§5.9.5, default depth 10), now extended from
  rules to transitions. A cycle (A → B → A) aborts at the depth cap rather than
  looping.
- **Locks are bounded by the budget** and follow the lock-ordering / deadlock-retry
  rules of §5.9.7 (parent before child, ascending by id; retry on SQLSTATE
  `40P01`/`40001`). A sync chain touching many records is a signal to use async
  instead — the Studio warns near the budget or for wide fan-out.
- Use for **immediate, in-process** state progressions (Approve → auto-Fulfill where
  Fulfill is pure data manipulation).

### Async chains (cross-system / long-running / external; eventual, with required failure handling)

A transition whose successor is cross-system, long-running, or waits on an external
event emits a domain event to the outbox; the receiver runs later, **timer-style**:
the worker loads the record `FOR UPDATE`, re-checks the current state, and proceeds
or aborts (§5.9.6). Delivery is at-least-once (§5.9.4); idempotency comes **for free
from the state-guard re-check** — re-running Fulfill on an already-Fulfilled record
fails the "current state == Approved" guard and is a no-op.

Because async chains **may partially complete**, the modeler **must declare failure
handling** as part of the transition definition — the engine rejects an async chain
without it:

- a **`failure_state`** (a designated state on the workflow, not new schema) the
  record moves to on exhaustion;
- a **retry policy** (backoff + jitter, max attempts — from the outbox, §5.9.4);
- an **optional compensation** action (saga-style) to undo side effects of the steps
  that did commit.

On exhaustion the record transitions to `failure_state` and an alert fires. **There
is no silent partial completion.**

### Visibility

Every transition attempt and its outcome (committed / pending / failed) is written
to `sys_event_log` (§5.10), so partial completions are always auditable and queryable
by record state — never discovered only when a user notices a stuck record.

## Consequences

- **(+)** Atomicity is restored for the common immediate-chain case — Approve →
  auto-Fulfill is all-or-nothing by default, eliminating the manufactured
  partial-completion hazard.
- **(+)** Async chains remain for genuinely eventual work, but with explicit,
  required failure handling — partial completion is a modeled, alertable state, not a
  silent bug.
- **(+)** Reuses machinery already specified: the recursion budget, lock ordering,
  deadlock retry, timer serialization, and at-least-once/idempotent outbox delivery.
- **(+)** Idempotency is free (state-guard re-check), not a separate dedupe log.
- **(−)** Sync chains hold locks across multiple records in one transaction — bounded
  by the recursion budget, but the Studio must guide modelers to async for wide/deep
  or external-touching chains.
- **(−)** Async chains with compensation add modeling burden (the modeler must design
  failure states/compensations). This is inherent to correct distributed workflows —
  unavoidable for cross-system sagas — and the engine enforces that the work is done
  by rejecting unhandled async chains at publish (§5.8).
