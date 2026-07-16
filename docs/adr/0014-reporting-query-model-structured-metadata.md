# ADR-0014: Reporting query model — structured metadata (not SQL), runner-context AuthZ by construction

- **Status:** Accepted
- **Date:** 2025-07-16
- **Detail:** PLAN.md §5.17

## Context

`md_report_dataset` was specified only as "query + params + joins." That leaves an
open attack and correctness surface larger than the carefully-bounded expression
DSL (§5.2):

- **What language is the query?** If it is raw SQL, it is a bigger hole than
  expressions: SQL injection, cartesian-product DoS, and a path that bypasses
  field/record security.
- **Author vs runner AuthZ is unspecified.** The admin who authors a report and
  the regular user who runs it — whose object/field/record permissions apply? It
  must be the runner's, but the rule was not stated, and reports commonly read
  fields the runner cannot see (e.g. `salary`).
- **Cost is unbounded.** Reports joining and grouping over large tables, or
  touching non-hoisted JSONB attributes, can full-scan. §5.16's "list queries are
  paged and cost-limited" does not cover report datasets.

## Decision

A report query is a **structured metadata model**, never raw SQL, compiled to
parameterized SQL over the `biz` tables by the engine. Because the engine builds
the SQL, it enforces security **by construction** and bounds cost.

- **Model:** `md_report_dataset` = `base_entity` + `fields[]` (each a reference
  *traversal* + field + alias + optional aggregate) + `filters`/`having` as the
  **bounded expression DSL AST** (§5.2) + `group_by` + `order_by` + `parameters` +
  `limit`. Reference traversals resolve to real joins over hoisted FK columns
  (§5.1/§5.7). **No raw-SQL report path in v1**; the escape hatch is a `wasmtime`
  extension (§5.6) or a deferred capability-flagged raw-SQL feature.
- **AuthZ = the runner's, always.** The author's permissions gate only who may
  *edit* the report. At run time the engine enforces, structurally:
  - *Object* — runner needs `read` on every entity in the traversal.
  - *Field (projection)* — invisible `select` fields are **dropped**.
  - *Field (semantic)* — an invisible field in `filter`/`group_by`/`having`/
    `order_by` is a **run-time error** (silent drop would change semantics and
    could reveal rows).
  - *Record* — the runner's record-security predicate (§5.11/ADR-0013) is injected
    **at every entity in the traversal, not only the base**, so a join cannot leak
    a record the runner cannot read.
  - *Aggregates* inherit FLS (an aggregate over `salary` is as sensitive as
    `salary`).
- **Cost control:** per-tenant cost estimate + budget (over → refuse, or run async
  as an apalis job with a download link, §5.13); result cap + timeout; non-hoisted
  JSONB access flagged (hoist at publish, §5.8, or mark async-only); join-depth
  limit (as GraphQL, ADR-0010).
- **Scheduled reports** run under a configured **running user** (default: the
  schedule owner); recipients receive that fixed, filtered output. Revoking the
  running user's access (ADR-0013) stops the schedule from leaking.

## Consequences

- **(+)** No SQL injection and no DoS-by-construction (the engine emits only
  structured, parameterized SQL with bounded DSL predicates).
- **(+)** Field- and record-level security cannot be bypassed — the engine never
  emits SQL for fields/rows the runner cannot see. Reuses the DSL, hoisting, and
  sharing (ADR-0013) machinery rather than reinventing it.
- **(+)** Reports degrade gracefully per-runner (columns drop) rather than failing
  opaquely; the Studio can preview who can run what.
- **(−)** Less expressive than raw SQL (deliberate). Power users who hit the limit
  need the `wasmtime`/deferred path.
- **(−)** Reports referencing non-hoisted JSONB attributes may require a publish
  to hoist (§5.8) before they perform — surfaced by the cost estimator, not
  discovered in production.
