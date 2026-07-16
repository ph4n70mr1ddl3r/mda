# ADR-0012: Value-constraint composition across multiple roles (intersection)

- **Status:** Accepted
- **Date:** 2025-07-16
- **Refines:** ADR-0005 (grain 6 — value constraints)
- **Detail:** PLAN.md §5.11

## Context

ADR-0005 defines six authorization grains and states the model is "purely
additive on top of a deny-by-default baseline — no negative permissions." That is
correct and important for the *binary* grains (object / field read-write / action
permission: can the user do it at all). But `sec_field_constraint` (grain 6) is a
per-role **predicate on an already-granted write** ("role=sales_rep ⇒
discount ≤ 0.05"), not a binary grant, and its composition rule across a user's
multiple assigned roles was unspecified.

The naive reading — "additive = union" — is a **security hole**. If a user holds
role A (constraint: discount ≤ 0.05) and role B (no constraint on discount), a
union rule lets B's permissiveness override A's restriction, so the user writes
`discount = 0.50`. The constraint becomes bypassable simply by adding (or holding)
a permissive role — unacceptable for an enterprise authorization model.

The tension is only apparent: "no negative permissions" governs whether a
*capability* is granted; it says nothing about how *conditions on a capability*
compose. That rule must be stated explicitly.

## Decision

Separate **capability** from **condition**, and compose each correctly:

- **Binary capability** (tenant / object / field read-write / action) — **union**
  across assigned roles. If any role grants it, the user has it. This is the
  additive, no-negative-permission model.
- **Write-value constraint** (`sec_field_constraint`, grain 6) — **intersection**
  across all roles that grant write on `(entity, field)`. A write is permitted
  only if it satisfies *every* applicable constraint. A role that grants only
  `read` (not `write`) on the field imposes no write constraint.
- **Universal validations** (`md_rule` / field validations) — always apply
  regardless of role, and intersect with the per-role constraints and with each
  other.

Formally, for a user U writing value `v` into field F on entity E:

```
permitted_value(v) =
    can_write(U, E, F)                                              # union of write grants
  ∧ ( ∀ role r ∈ roles(U) where r grants write(E,F): constraint_r(E,F,v) )   # intersection
  ∧ ( ∀ universal validation u on (E,F): u(v) )                              # always-on
```

This is **not** a negative permission. A negative permission *revokes* a binary
grant ("deny write"). A value constraint does not revoke the write capability —
the user can still write the field — it defines the predicate under which each
role's grant is exercised. Composing conditional grants has always required
intersecting their conditions; this ADR makes that explicit and pins the secure
default.

## Consequences

- **(+)** A per-role value constraint cannot be bypassed by adding (or holding) a
  more permissive role. Guaranteed by construction.
- **(+)** Clean formal model: capability = union, conditions = intersection — the
  standard composition of conditional grants in access-control theory, and
  consistent with universal validations (also intersected). One mental model for
  "what values may be written."
- **(+)** Preserves "no negative permissions" where it belongs (binary
  capability) without weakening security where composition matters (conditions).
- **(−)** Adding a role can *narrow* the set of writable values (a second role may
  bring an additional constraint). This is counterintuitive next to the additive
  *capability* model and must be documented for modelers — but it is the correct,
  expected behavior for conditional grants and the secure default.
- **(−)** The effective-context cache (§5.11) must compile, per `(entity, field)`,
  the AND of all applicable write constraints drawn from the user's
  write-granting roles — not merely evaluate each role independently. One-time
  compilation per session, cached and invalidated on role/constraint change.
