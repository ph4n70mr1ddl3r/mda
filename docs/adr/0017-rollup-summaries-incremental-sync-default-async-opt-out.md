# ADR-0017: Rollup summaries — incremental synchronous by default, async opt-out for hot parents

- **Status:** Accepted
- **Date:** 2025-07-16
- **Detail:** PLAN.md §5.7

## Context

§5.7 declares `md_relationship.rollup_summary` — an aggregate
(count/sum/avg/min/max) of a child field stored on the parent, enabled for
`master_detail` — but never specifies **when or how** the aggregate is computed or
stored. This is a non-trivial decision:

- It collides with §5.9's "keep transactions short" and "no cascade locks": the
  obvious implementation (recompute the parent on every child write) both contends
  on the parent row and mishandles min/max.
- It has a classic failure mode — **parent-row contention under high fan-in**
  (many children of one parent written concurrently, e.g. a busy customer's
  invoices), which serializes on the parent.
- It interacts with cascade deletes (ADR-0006): native `ON DELETE CASCADE` removes
  children at the DB layer and bypasses any app-layer delta logic, so a naive
  rollup goes stale when children are cascade-deleted.

Left unspecified, an implementer will guess, and the guess is usually wrong.

## Decision

1. **Stored, not virtual.** A rollup is a real hoisted column on the parent — it is
   queried, filtered, and indexed like any field. It is never recomputed on every
   read.

2. **Incremental, synchronous, in the child's write transaction — by default.** On a
   child insert/update/delete the engine applies an O(1) delta to the parent's
   rollup column within the same transaction (this is the "update related rows"
   side-effect of unit-of-work step 5, §5.9.3):
   - `count`: ±1
   - `sum`: ±(new − old) child value
   - `avg`: stored as `sum` + `count` (avg derived) — same delta cost
   - `min`/`max`: cannot be updated by subtraction; on a child delete of the
     current extremum, mark the rollup dirty and recompute (bounded re-scan of the
     children), still in-transaction for small child sets. Large sets → use the
     async opt-out below.

   This keeps the rollup transactionally consistent with the child write at the
   cost of a brief lock on the parent row.

3. **Async opt-out for hot parents (high fan-in).** A relationship may declare
   `rollup_mode = async`. The child write then emits a "parent rollup dirty" row to
   the transactional outbox (§5.9.4) instead of locking the parent; a worker
   recomputes the rollup. The rollup is **eventually consistent** for that
   relationship — acceptable for display/summary, **not** for a rollup that drives a
   synchronous workflow guard, a uniqueness constraint, or a security decision
   (those must stay `sync`).

4. **Lock ordering.** A sync rollup update is a two-row write (child + parent).
   Acquire the parent lock before the child (parent-before-child, §5.9.7) to keep
   deadlock ordering consistent system-wide; retry on SQLSTATE `40P01`/`40001`.

5. **Cascade-delete interaction (ADR-0006).** Native `ON DELETE CASCADE` removes
   children at the DB layer, bypassing the app's delta logic. Therefore a
   `master_detail` relationship that carries a rollup either installs a DB trigger
   or has the engine's delete path handle the cascade set explicitly, so the
   parent's rollup is decremented/recomputed for cascade-deleted children
   (equivalently: mark dirty + async recompute post-cascade). Without this the
   rollup goes stale on cascade.

6. **Security.** A rollup column is a field on the parent and inherits the parent's
   FLS and record-security (§5.11). No special path.

7. **Interaction with record sharing (ADR-0013).** A sync rollup update mutates a
   *parent* field inside the *child's* write transaction (§5.9.3 step 5), but the
   per-record share-recompute in step 6 only touches **the record being written**
   (the child). If a sharing rule keys off the parent's rollup (e.g. "share Account
   where `total_invoices` > 10k with Finance"), the parent's `sec_record_share` rows
   would otherwise go stale until the parent is next written or an admin recompute
   runs. Therefore: **a sync rollup delta that lands on a sharing-rule-relevant
   parent field must also trigger that parent's synchronous share recompute** in the
   same transaction (extend step 6 to the affected parent(s)). An *async* rollup
   (`rollup_mode = async`) is eventually consistent by definition, so its
   share-recompute follows the same eventual path — acceptable for display rollups,
   **not** for a rollup that drives a security decision (keep those `sync`).

## Consequences

- **(+)** Rollups are consistent by default (sync, incremental, in-txn) without
  recomputing on read.
- **(+)** O(1) per child write for count/sum/avg; bounded re-scan only for min/max
  extremum loss.
- **(+)** A hot-parent escape hatch (async) for high-fan-in relationships, with an
  explicit eventual-consistency caveat.
- **(+)** Consistent with §5.9 (incremental = short lock) and ADR-0006 (cascade
  handled).
- **(−)** Sync rollups contend on the parent row under concurrent fan-in — the
  async opt-out exists for exactly this; the Studio should warn when a sync rollup
  is declared on a high-volume child entity.
- **(−)** min/max recompute on extremum delete can touch many rows; large child
  sets should use async mode.
- **(−)** Cascade-rollup maintenance adds a trigger/delete-path responsibility —
  real but localized.
