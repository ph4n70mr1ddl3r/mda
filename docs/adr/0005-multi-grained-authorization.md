# ADR-0005: Multi-grained authorization

- **Status:** Accepted
- **Date:** 2025-07-16
- **Resolves:** REVIEW.md C6
- **Detail:** PLAN.md §5.11

## Context
"Verb on entity" is too coarse for enterprise: it omits field-level, action/transition-level, and record-sharing granularity. ABAC (the expression engine) is the right primitive, but the permission *model* must be multi-grained.

## Decision
Six grains: **tenant** (Postgres RLS), **object** (`sec_permission`), **record** (deny-by-default OWD + ownership + team + sharing rules + manual shares + optional role hierarchy, enforced as query-rewrite predicates in `mda-data`), **field** (`sec_field_permission`: none/read/write), **action/transition** (`sec_action_permission`), **value** (`sec_field_constraint`, ABAC). Additive only — no negative permissions. Effective context is cached per session.

## Consequences
- **(+)** Enterprise-grade access control; one enforcement map spanning REST and the event channel (ADR-0004).
- **(−)** Business row security runs in the app layer (RLS kept for tenant isolation only); sharing needs a materialized `sec_record_share` table, recalculated on write/rule change.
