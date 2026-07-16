# ADR-0013: Sharing materialization — revoke-safe via epoch invalidation

- **Status:** Accepted
- **Date:** 2025-07-16
- **Refines:** ADR-0005 (record-level security, §5.11.3)
- **Detail:** PLAN.md §5.11

## Context

ADR-0005 §5.11.3 materializes record visibility into
`sec_record_share(record_id, principal_id, access)` and states it is
"recalculated when a record is written or a sharing rule changes … Rule/hierarchy
evaluation is pushed into this table asynchronously (via the outbox) so writes
stay fast." Two consequences were left unresolved, and one is a security exposure:

1. **Revocation lag = a confidentiality window.** If recomputation is purely
   eventual, then between a *narrowing* change (a sharing rule edited to be
   stricter, a role hierarchy re-parented, a team membership removed) and the
   async reshare completing, stale rows in `sec_record_share` still grant access.
   For a deny-by-default model this is a **data leak**: a user retains access
   *past* revocation. Grant-lag is a mere availability nuance (a user temporarily
   sees less than they are entitled to); revoke-lag is a breach. The two must not
   be treated symmetrically.
2. **Thundering herd.** Editing a broad sharing rule re-evaluates every matching
   record — potentially millions — in one kick. "Recalculate on rule change"
   without bounds is an availability and correctness hazard (long-held work,
   partial state, retry storms).

The asymmetry that resolves both: **it is always safe to under-grant temporarily;
it is never safe to over-grant temporarily.** The materialized table may lag on
the *grant* side (eventual, progressive) but must be *instantly correct* on the
*revoke* side.

## Decision

Three rules, in priority order.

### 1. Revoke by invalidation, not by recomputation (the core mechanism)

Every criteria-based sharing rule carries an **`epoch`** (monotonic, per rule per
tenant). Every materialized `sec_record_share` row records the `epoch` under which
it was computed. Enforcement honors a share **only if its epoch is current**:

```
rule_visible(U, R) =
    EXISTS sec_record_share(
        record_id = R, principal ∈ principals(U), access ≥ needed,
        rule_id = K, epoch = E)
    WHERE E ≥ current_epoch(K)        -- stale shares are ignored, never honored
```

Bumping `current_epoch(K)` is a **single O(1) update** to one `sec_sharing_rule`
row — it invalidates *all* shares computed under the old epoch instantly, without
touching `sec_record_share`. Recomputation then runs async to write new-epoch
rows; until it does, the safe under-grant holds (stale shares filtered out).
**There is no window in which a revoked share is honored.**

Generalized to the role hierarchy: a per-tenant `hierarchy_epoch`; hierarchy-derived
shares carry it; a re-parent bumps it (instant revoke) + async recompute.

### 2. Split recompute by trigger — synchronous per-record, async for admin-rule changes

- **Record write** (create/update with owner or field change) → recompute **that
  one record's** shares **synchronously, in the write transaction**. It is
  O(number of rules) for a single record — bounded — so it stays within the
  transaction's latency budget (§5.9). A record's own shares are therefore always
  fresh immediately after its write; **there is no per-record revocation lag at
  all.**
- **Admin change** (sharing-rule edit, hierarchy re-parent, OWD change,
  large-scale team-membership change) → bump the affected epoch(s) (instant revoke,
  O(1)) and enqueue a **batched, resumable recomputation job** (apalis, ADR-0007;
  checkpointed like `md_migration_log`). Grant-side catch-up is progressive;
  revoke-side correctness is already guaranteed by the epoch.

The *only* eventual path is admin-driven bulk change, and even there revocation is
immediate; the async work is purely grant-side catch-up.

### 3. Avoid epoch bumps for pure grants; bound the thundering herd

- **Adding** a sharing rule, a team member, or a manual share is purely
  *additive* — it can never revoke — so it does **not** invalidate existing shares;
  recomputation only *adds* new-epoch rows. This handles the most common broad
  change (someone adds a sharing rule) with no blip.
- **Editing** an existing rule's condition is treated as revoke-conservative: bump
  the epoch (safe), recompute. Detecting a "strictly broader" condition edit is
  undecidable in general, so the safe default wins.
- Bulk recomputations are **batched, parallelized, idempotent, and resumable**; the
  Studio surfaces progress + ETA (as with publish staging, ADR-0011) and
  rate-limits concurrent reshares per tenant. A failed/aborted reshare leaves
  partial-but-valid (under-grant) state — never over-grant.

### Enforcement composition (where each layer is evaluated)

```
visible(U, R) =
    owd_visible(U, R)            # live, cheap: owner / owner's team per OWD
  ∨ manual_share(U, R)           # live, cheap: sec_share lookup
  ∨ hierarchy_visible(U, R)      # materialized, hierarchy-epoch-gated
  ∨ rule_visible(U, R)           # materialized, rule-epoch-gated  (above)
```

Ownership / team / manual are evaluated **live** from the (cached) effective
context — they are cheap, change with the record or the user's team set (already
cached), and need no materialization. Only criteria-rules and hierarchy are
materialized, and only they carry the epoch gate.

### Cross-instance promptness

Epoch bumps land in the DB; a `meta_changed` / event-channel broadcast (§5.3,
§5.10) invalidates cached effective contexts on **all** instances so compiled rule
predicates refresh promptly. Correctness does not depend on the broadcast (the next
enforcement read sees the new epoch regardless), but the broadcast keeps revoke
latency low across the cluster.

## Consequences

- **(+)** **No revocation leak window, ever.** Stale shares are filtered by the
  epoch predicate the instant a rule/hierarchy bumps; the materialized table is
  never trusted beyond the current epoch.
- **(+)** Per-record revocation is *synchronous* (recompute in the write txn) — the
  common case has zero lag.
- **(+)** Grant-side catch-up stays async and fast for writers; bulk admin changes
  do not block writes.
- **(+)** Additive grants (the common broad change) need no invalidation — no
  availability blip.
- **(−)** The enforcement join carries an epoch predicate (minor cost; covered by
  the `(record_id, principal_id)` index, and the epoch is a cheap integer compare).
  A GC job removes superseded old-epoch rows after recomputation completes.
- **(−)** After a *narrowing* admin rule edit affecting many records,
  legitimately-entitled users see a temporary under-grant (availability, not
  security) until batched recomputation catches up — surfaced as progress in the
  Studio. This is the deliberate, correct trade: confidentiality is never
  compromised; availability degrades briefly and recovers.
- **(−)** Extra schema: `epoch` on `sec_sharing_rule` and `sec_record_share`, plus a
  per-tenant `hierarchy_epoch`; a reshard job and a stale-row GC job. Real but
  bounded surface area.
