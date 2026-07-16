# ADR-0001: Storage model & referential integrity (Pattern B)

- **Status:** Accepted
- **Date:** 2025-07-16
- **Resolves:** REVIEW.md C1
- **Detail:** PLAN.md §5.1, §5.7; `docs/ri-strategies.md`

## Context
A model-driven system can store business data as (A) shared/universal tables, (B) one real table per entity with native FKs, (C) real tables with soft references, or (D) JSONB documents. The choice dictates how referential integrity is enforced: only B gives native `FOREIGN KEY` constraints; the rest force app-layer RI. Salesforce chose A only because of an extreme multitenancy constraint we do not have.

## Decision
Adopt **Pattern B**: one real `biz.<entity>` table per entity, generated at publish, with a stable core schema + hoisted relational columns (a real `FOREIGN KEY` per relationship; `DEFERRABLE` for mutual references) + hoisted scalar columns (or generated columns) for indexed/unique fields + a JSONB `attributes` payload for the rest.

## Consequences
- **(+)** Native, trustworthy RI (cascades, restricts) for free via Postgres.
- **(+)** Natural SQL joins for reporting.
- **(−)** Requires a DDL/migration engine at publish time (also needed for data migration — ADR-0002). Accepted cost.
- **(−)** Reference fields are always hoisted to real columns; never stored only in JSONB.
