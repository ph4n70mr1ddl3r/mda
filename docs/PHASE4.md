# Phase 4 — Expression Engine & Rules (status & handoff)

**Status: complete & verified.** Implements PLAN §9 Phase 4: the bounded
expression DSL, set-field business rules, and calculated fields, all firing
synchronously in the write transaction. Deliverable met: *define a rule "when
status changes to Closed, set closed_at = now()"; it fires.*

## What was built

**`mda-expression`** (PLAN §5.2):
- JSON-AST expression (`Lit`, `Field`, `Cmp`, `Arith`, `And`/`Or`/`Not`, `If`,
  `Call`) + a **bounded evaluator** (max depth 32, step budget 10 000, node-typed
  comparisons) so a pathological/hostile expression cannot DoS the system (U6).
- A pure `Registry` of allowlisted functions: `now`, `today`, `len`, `upper`,
  `lower`, `coalesce`, `concat`. 6 unit tests.

**`mda-rules`** (PLAN §4.3 / §5.9):
- `md_rule` table; `load_active(tenant, entity)`; `fire(event, ctx)` for
  **set_field** actions (condition + value both DSL expressions), applied in
  record-context order.
- `compute_calculated` — a field whose `config.formula` is an expression is
  recomputed on every write (same-record formula, §5.7).

**Wiring** (`mda-api/data.rs`): on create/update, after RBAC + FLS checks, the
record context is built, `after_create`/`after_update` rules fire and calculated
fields recompute, then the (derived) values are written in the same operation —
synchronous and in-transaction (§5.9). For updates, the existing record is
merged into the condition context (core columns excluded).

## Verification (all green)

`fmt` · `clippy -D warnings` · expression unit(6); schema(1); studio(3);
data(5, incl. `rules_and_calculated_fields_fire`: calculated `total = qty*price`
on create; rule sets `closed_at = now()` when `status → Closed`).

## Phase-4 decisions / deferrals

- **Phase 4 scope** = set-field rules + calculated fields (the deliverable).
  Deferred: other action kinds (call-function, fire-event, enqueue), field-level
  **validations** as first-class DSL rules, and **async outbox** side-effects
  (webhook/email/notification) — those land with notifications/outbox.
- Rule authoring UI is the Studio (Phase 8); rules are inserted via SQL/metadata
  for now.
- Rules are loaded per request (caching follows the metadata cache later).
