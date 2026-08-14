# ADR-0025: Team hierarchy — ancestor-team visibility (`sec_team.parent_id`)

- **Status:** Accepted
- **Date:** 2025-08-14
- **Refines:** ADR-0013 (record-level security, `owd_visible`), §5.11.3
- **Detail:** PLAN.md §5.11 / §14

## Context

ADR-0013 introduced flat team-OWD (`owd_visible`): when an entity's org-wide
default is `team`, a member of a team may read records owned by members of that
*same* team (write stays owner-only, mirroring `PublicRead`). The hierarchy
refinement — "a manager's team sees descendant teams' records" — was explicitly
deferred in `docs/CAPABILITIES.md` and §14 as needing "the role/team-hierarchy
epoch machinery."

The schema already carried `sec_role_hierarchy` (parent/manager *role* edges,
still unused) but no parent edge on `sec_team` itself, so there was no tree to
walk. The result: a platform feature that enterprises expect (regional managers
see their regions' reps' records) was absent, and the security graph (teams,
roles, permissions, OWD, users) was only editable through the DB or the tenant
config import — there was no operator-facing management surface at all.

## Decision

### 1. A self-referential `parent_id` on `sec_team` (not role hierarchy)

`sec_team.parent_id UUID REFERENCES sec_team(id)` is the visibility tree. NULL
roots a team. This is *team* hierarchy (who-reports-into-whom), deliberately not
the *role* hierarchy of `sec_role_hierarchy` (which is about permission rollup
and remains a separate concern). Cycles are prevented in app code (the admin API
rejects a parent whose ancestor chain passes through the child); a stray cycle
would be harmless to the recursive descent (it just re-visits a known set).

### 2. Visibility flows DOWNWARD; the predicate walks the tree at query time

Under team-OWD, a viewer may read a record when the record owner's team is
reachable *downward* from the viewer's team. The record-visibility predicate in
`mda-data::read_predicate` injects a `WITH RECURSIVE descendant_teams` descent
from the viewer's team `${t}` and admits the owner when their `team_id ∈` that
set. Because the descent is anchored on a single bind (the viewer's team), no
correlation is needed and the recursion is self-contained. **Flat collapses to
same-team-only:** with no `parent_id` edges set, the descent yields just `${t}`,
exactly the prior `owd_visible` behavior — so existing tenants are unaffected.

Write stays owner-only (team-OWD never grants write beyond the owner), so the
hierarchy is read-visibility only, mirroring `PublicRead`.

### 3. Recipient resolution walks UPWARD (the dual)

`resolve_record_readers` (the `record_readers` notification strategy) computes
the *inverse*: who can read *this* record. It walks the tree *upward* from the
owner's team (`WITH RECURSIVE ancestor_teams`), so a record owned in a sub-team
notifies members of the owner's team **and every ancestor (manager) team**. This
is the exact dual of the predicate and keeps the two consistent by construction.

### 4. An admin security-graph API makes it operable

A new superuser-only surface (`/api/admin/*`) manages the whole graph: teams
(CRUD + `parent_id` re-parent with a cycle/self-loop guard), roles (CRUD +
object/field permission grant/revoke), OWD (per-entity set), users (CRUD +
activate/deactivate + password reset + role assignment). Nullable PATCH fields
(`parent_id`, `team_id`) use the `Option<Option<T>>` + `deserialize_some` pattern
so an explicit `null` clears the column while an omitted key leaves it
untouched. Every handler is superuser-gated (the security graph is the trust
root) and runs under the tenant GUC so the `sec.*` RLS policies engage.

### 5. The hierarchy round-trips through tenant config import/export

`restore_teams` now builds a bundle-id → actual-id map and re-links `parent_id`
in a second pass (a parent may be declared after its child in the bundle), so a
tenant backup snapshot preserves the tree.

## Consequences

- **+** The single most-requested enterprise record-security behavior ("managers
  see below") is now live, sharing the same OWD predicate as flat team-OWD.
- **+** The security graph is operable for a real operator without DB access.
- **+** No migration of existing data: flat tenants keep their behavior.
- **−** The recursive descent adds a small per-query cost proportional to team
  depth; bounded by team-tree fan-in (an indexed `(tenant_id, parent_id)` keeps
  it cheap) and acceptable for the org-chart-shaped trees this models.
- **⊘** Role hierarchy (`sec_role_hierarchy` permission rollup) remains unused —
  a separate, permission-grain concern, not addressed here.
