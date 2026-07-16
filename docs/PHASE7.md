# Phase 7 — Reporting (status & handoff)

**Status: complete & verified (core).** Implements PLAN §5.17: a structured
report dataset compiled to parameterized SQL over `biz.<table>`, with the
**runner's** object/field/record security enforced **by construction**. CSV
export included.

## What was built

**`md_report`** table (`migrations/20260106000001`) — a report = name + a
structured `dataset` JSONB.

**`mda-reports`** (PLAN §5.17):
- `Dataset { base_entity, fields[], filters[], group_by[], order_by[], limit }`;
  `SelectField { field, aggregate(count|sum|avg|min|max), alias }`.
- `run(identity, dataset)` compiles to `SELECT jsonb_build_object(...) FROM
  biz.<base> WHERE <tenant + record-scope + filters> [GROUP BY …] [ORDER BY …]
  [LIMIT]`, parameterized.
- **AuthZ by construction**: object (needs `read`); field projection (unreadable
  selects dropped); **field semantic** (an unreadable field in `filter`/
  `group_by`/`order_by` → 422, never silent — a dropped filter/group could
  reveal rows); **record** (runner's ownership/OWD predicate injected).
- `to_csv` renderer.

**API** (`mda-api/reports.rs`): `GET /api/reports/:id/run`, `GET
/api/reports/:id/export` (CSV). `POST /api/reports` authoring is the Studio
(Phase 8); reports are inserted via metadata for now.

## Verification

`--test data report_runs_with_grouping_and_export`: 3 Customers (2 Bronze / 1
Silver) → a count+sum-by-tier report returns the right grouped rows; CSV export
returns 200.

## Phase-7 decisions / deferrals

- **Single-entity reports** (reference-traversal joins — `invoice.customer.name`
  — are a follow-up; §5.17 "at every entity in the traversal").
- Deferred: **parameters** (`:region`), **scheduled reports** + email delivery,
  **PDF/XLSX** renderers, dashboards, and a cost-estimate/budget (currently a
  plain LIMIT).
