# ADR-0011: Publish execution — staged migration + atomic cutover

- **Status:** Accepted
- **Date:** 2025-07-16
- **Refines:** ADR-0002 (publish execution model)
- **Detail:** PLAN.md §5.8

## Context

ADR-0002 established the draft → validate → publish → activate lifecycle and the
op classification (additive / transforming / two-phase destructive). It left the
*execution mechanics* under-specified, and §5.8 as written asserts two properties
that cannot both hold:

- **Atomicity** — "Begin transaction … run DDL + data migrations … apply diff to
  `md_*` … bump `md_active_version` … Commit" is presented as one transaction.
- **Resumability** — "each step logged to `md_migration_log` for resume/revert."

These conflict. A single transaction is all-or-nothing: on failure it rolls back
wholesale, so there is nothing to *resume* from. Conversely, resumability requires
multiple transactions with checkpointed progress — which means a publish is
*not* atomic and a partially-applied model can be observed mid-flight.

The conflict is harmless for **additive** and **small** transforms (they finish in
a sub-second transaction). It becomes acute for **large transforming ops** — e.g.
casting a `text` column to `numeric` on a 10M-row table, or building a unique
index over it. Holding one transaction open for the duration is unacceptable:
ACCESS EXCLUSIVE / long-held locks, WAL bloat, replication lag, idle-in-transaction
stalling vacuum. This is the classic online-schema-change problem and the place
homegrown low-code platforms fail silently.

We need a publish execution model that is **atomic where it must be** (the model
is either old or new, never half) and **resumable where it has to be** (the
expensive data-movement work), without claiming both for the same operation.

## Decision

Split publish execution into two phases — a resumable **staging** phase that is
invisible to the runtime, and a short **atomic cutover** — using the
**expand / contract** discipline. The runtime continues to serve the *old* active
model (via `md_active_version`, unchanged) throughout staging; only the cutover
flips the model, and the cutover transaction is kept short enough to be genuinely
atomic.

### Phase A — Staging (resumable, batched, NOT visible)

- The draft enters status `publishing`. Enforced **one `publishing` draft per
  tenant** (unique partial index on `md_draft(tenant_id) WHERE status='publishing'`)
  — this replaces a long-held advisory lock as the serialization gate for Phase A.
- A background job (apalis, ADR-0007) executes the **large/transforming** ops only,
  each as an **expand** step on the existing `biz.<table>` (same table, not a
  shadow table — the core schema is fixed per §5.1, so wholesale table rebuilds do
  not arise in v1):
  - **Type change** → `ADD COLUMN _v2_<name> <newtype>`, then batched backfill
    `UPDATE … SET _v2_<name> = <cast>(<name>) WHERE id BETWEEN … AND _v2_<name> IS NULL`,
    with an on-failure policy per row (fail-fast / null / sentinel) logged to
    `md_migration_log`.
  - **Make required** → `ADD CONSTRAINT _v2_nn CHECK (<name> IS NOT NULL) NOT VALID`,
    then `VALIDATE CONSTRAINT` (non-blocking; PG 12+). `SET NOT NULL` itself is
    deferred to cutover, where it becomes metadata-only because the check is
    already validated.
  - **Add required field** → batched backfill of the default/constraint, same as
    above.
  - **New index / unique constraint** → `CREATE INDEX CONCURRENTLY` (cannot run
    inside a transaction, so it *must* be a Phase A step, never cutover). For a
    unique constraint, build the unique index concurrently first, then attach it
    as a constraint during cutover (`ADD CONSTRAINT … USING INDEX`).
- Every batch is its own transaction; progress is checkpointed in
  `md_migration_log` (op, last-processed id, status, rows affected). **Resume on
  failure** = restart the job; it picks up at the last checkpoint. **Abort** =
  drop the `_v2_*` columns/indexes; the old model is untouched.
- **Backfill race** — rows inserted by the runtime (still using the old model)
  during Phase A arrive with `_v2_* = NULL`. Handled by a **final delta backfill
  inside the cutover transaction** (under the advisory lock, see Phase B): catch
  up the tail `WHERE _v2_<name> IS NULL` before the swap. For tables too hot for
  even that, Phase A may install a temporary trigger to keep `_v2_*` in sync;
  the trigger is dropped at cutover. Delta-at-cutover is the default; trigger is
  the escape hatch for very-high-write tables.

### Phase B — Cutover (atomic, short)

- Acquire `pg_advisory_xact_lock(<tenant_key>)` **inside** the cutover transaction
  (short-lived; serializes only the flip, not Phase A).
- **Time budget: the cutover transaction must complete in single-digit seconds**
  (target < 5s of lock hold). Any op that cannot meet this is *staged in Phase A*
  and reduced to metadata-only at cutover. This is the rule that keeps the model
  genuinely atomic.
- Within the transaction:
  1. Final delta backfill for any staged column still NULL (closes the backfill race).
  2. **Contract** — rename old column → `_<name>_old` (kept for a grace window,
     not dropped), rename `_v2_<name>` → `<name>`; attach staged indexes as
     constraints (`USING INDEX`); apply `SET NOT NULL` (metadata-only post-validation).
  3. Apply the additive ops (e.g. `ADD COLUMN` with default — metadata-only on
     PG 11+) and the metadata-level part of destructive ops (**retire** only;
     purge stays deferred per §5.8).
  4. Apply the diff to the `md_*` metadata tables.
  5. Archive previous active model to `md_snapshot`.
  6. Bump `md_active_version`.
  7. Commit; broadcast `meta_changed` (LISTEN/NOTIFY + Redis).
- On failure → transaction rolls back; `md_active_version` unchanged; staged
  `_v2_*` artifacts remain and the cutover can be retried or aborted-with-cleanup.

### Post-publish cleanup (deferred, reversible)

- The renamed `_<name>_old` columns and any superseded indexes are dropped by a
  scheduled job after a short grace window (default 24h). Keeping them briefly
  makes a bad cutover recoverable without a full snapshot rollback — symmetric
  with the retire/purge grace of §5.8 and the archive philosophy of ADR-0006.

### Rollback (after a successful cutover)

- Per §5.8: restore a prior snapshot by re-publishing it as a draft through the
  same Stage → Cutover path. **Caveat (carried from the plan review):** a
  reversing transform is not guaranteed lossless — e.g. `numeric(12,2)` → `integer`
  truncates, and rolling it back restores the *truncated* values, not the
  originals. The migration plan must flag lossy transforms at validate time so
  modelers know rollback fidelity is reduced.

### Threshold (what is "large")

- Decided at **validate** time from the row-count estimate already produced in
  §5.8. Ops below a per-tenant threshold (default ~100k rows or estimated
  transform > a few seconds) run **inside the cutover transaction** as
  metadata-only or trivial backfill; above the threshold they are staged.
- `CREATE INDEX`/unique-constraint builds are **always** staged (CONCURRENTLY
  cannot run in a transaction), regardless of table size.

## Consequences

- **(+)** Atomicity is preserved *where it matters*: the model served by the
  runtime is either the old or the new one, never an intermediate state, because
  cutover is short and all data movement is pre-staged.
- **(+)** Resumability is provided *where it is needed*: the expensive batched
  work in Phase A is checkpointed and restartable, with no user-visible impact
  (old model still served).
- **(+)** No long-held locks, no hour-long transactions, no replication-lag
  surprises from publish.
- **(+)** Bad cutovers are recoverable within a grace window (renamed-old columns)
  before resorting to snapshot rollback.
- **(−)** Publish is no longer instantaneous for large transforms: there is a
  `publishing` window (minutes to hours) during which the old model is live.
  This is acceptable and arguably desirable (it is the cost of correctness), but
  the Studio must surface publish progress and ETA from `md_migration_log`.
- **(−)** More moving parts (background staging job, grace-window cleanup job,
  staged-column naming convention, backfill-race handling) — a real subsystem,
  not a footnote. This is the budget C3 (REVIEW.md) demanded.
- **(−)** Lossy transforms reduce rollback fidelity (caveat must surface at
  validate time).
