# ADR-0026: Closing ADR-0013 — criteria sharing rules + live role hierarchy

- **Status:** Accepted
- **Date:** 2025-08-14
- **Refines:** ADR-0013 (sharing materialization), ADR-0025 (team hierarchy, live)
- **Detail:** PLAN.md §5.11.3 / Phase 6

## Context

ADR-0013 designed the full record-visibility composition — `owner ∨ manual share
∨ team-OWD ∨ role hierarchy ∨ criteria rule` — but only manual shares, flat
team-OWD, and (with ADR-0025) ancestor-team visibility shipped. `sec_share_rule`
and `sec_role_hierarchy` existed as schema without engines. Phase 6 explicitly
deferred them ("criteria-based sharing rules, role hierarchy, materialized
`sec_record_share` with epoch invalidation"). This ADR records the closure
decisions made while implementing that half.

## Decision

### 1. Sharing rules: materialized, epoch-gated, per-record synchronous recompute

Exactly the ADR-0013 mechanics, with no simplifications:

- **Materialization.** A rule (`entity`, `condition` in the bounded DSL,
  `principal_id` (user **or team**), `access`) writes `sec_record_share` rows
  carrying the rule's `epoch`. Enforcement honors a rule-derived row only while
  `rs.epoch = rule.epoch AND rule.active` — a subselect in the read/write
  predicate, so **bumping the epoch is an instant, O(1) revoke** of everything
  materialized under the old epoch (the materialized table is never trusted
  beyond the current epoch).
- **Per-record recompute is synchronous, in the write transaction** (create,
  update, restore, and mass actions, which reuse the single-record pipeline):
  delete the record's rule-derived rows, re-evaluate the tenant's active rules
  against the after-image, re-insert current matches. O(active rules) for one
  record — bounded, inside the latency budget. There is **no per-record
  revocation lag at all**: a record that stops matching loses its grant in the
  same commit as the edit.
- **Admin edits split by direction** (ADR-0013 rules 1–3):
  - *create* — never bumps the epoch (a purely additive grant cannot revoke);
    materializes existing matches in bounded keyset batches within the request;
  - *edit / deactivate* — bumps the epoch first (instant revoke), then
    re-materializes; deactivation additionally drops the rule's rows;
  - *delete* — the rule row goes, its shares cascade away instantly.
- **Resumable catch-up.** `POST /api/admin/share-rules/:id/recompute?from=<last
  id>&limit=<n>` re-materializes in keyset batches (bounded per call, max 50k
  rows) reporting `scanned/materialized/truncated/last_id`, so a huge entity is
  progressively reconciled without a long-held transaction. Revocation never
  depends on it (the epoch is authoritative); it is purely grant-side catch-up.
- **Manual shares win collisions** (`ON CONFLICT DO NOTHING`): a rule never
  downgrades or upgrades an explicit per-record grant.

### 2. Role hierarchy: evaluated LIVE, not materialized

ADR-0013 sketched a materialized `hierarchy_epoch` for the role hierarchy. We
instead evaluate it **live** in the read predicate, exactly like the ADR-0025
team hierarchy: a recursive CTE walks `sec_role_hierarchy` downward from the
viewer's roles, and a record is visible when its owner holds a role in that
descendant set.

Why the deviation is safe (and strictly better on the axis ADR-0013 cares most
about):

- **Revocation lag is zero by construction.** A re-parent, detach, or role
  removal is effective at the *next query* — there is no materialized table to
  go stale, so no epoch, no GC job, and no window at all. ADR-0013's entire
  epoch machinery exists to make materialized revocation instant; live
  evaluation simply *is* instant.
- **Cost is bounded and indexed.** The CTE walks `(tenant, role)` edges and the
  per-role assignment sets; role graphs are small (tens of roles) compared to
  record counts (the thing materialization exists to protect). This is the same
  trade ADR-0025 already accepted for team visibility, and the two clauses are
  symmetric in the predicate.
- **Read-only.** The hierarchy amplifies **read** only — mirroring `PublicRead`
  and team-OWD, write never inherits downward (an owner-or-write-share check
  still gates every mutation). Enforced by test.

### 3. Enforcement composition (the full §5.11.3 stack)

```
visible(U, R) =
    owd_visible(U, R)            # live: owner / owner's team per OWD (ADR-0025 tree)
  ∨ manual_share(U, R)           # live: sec_record_share, rule_id IS NULL
  ∨ rule_visible(U, R)           # materialized, rule-epoch-gated (this ADR)
  ∨ hierarchy_visible(U, R)      # LIVE: role-descendant owners (this ADR)
```

One predicate (`mda_data::read_predicate`) is injected into every read surface —
CRUD, list, GraphQL, reports (now reusing the same predicate instead of an
owner-only filter — reports previously *under*-granted: a shared record did not
appear), notifications' record-reader resolution, and mass-action target
resolution (write variant). Share principals match the user **or their team**
(`principal_id IN (user, team)`), so team-targeted rules work.

## Consequences

- **(+)** Phase 6's deferred record-security surface is closed; §5.11's
  composition is implemented end-to-end with the revoke-safety property ADR-0013
  demanded (under-grant transiently is acceptable; over-grant never).
- **(+)** Reports, GraphQL, notifications, and the UI now share one visibility
  predicate — no surface can disagree with another.
- **(+)** The admin security API (`/api/admin/share-rules`, `/api/admin/roles/
  :id/parents`) makes the whole graph operable; tenant export/import carries
  share rules (principal must exist in the target tenant — user-referencing
  rules from another tenant are skipped, never silently dangling) and the role
  hierarchy (ids remapped through the role map).
- **(−)** The read predicate carries two recursive CTEs (team + role) when the
  caller is non-public — acceptable at enterprise role-graph sizes, and the
  clauses are only compiled in when the scope requires them.
- **(−)** Share-rule materialization on admin create/edit is bounded per
  request; an entity larger than the bound needs the documented resumable
  `/recompute` loop (or the scheduler) to finish grant-side catch-up.
