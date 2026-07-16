# MDA — Model-Driven Architecture Enterprise Platform

A declarative, data-driven, model-driven **no-code enterprise system** built in Rust.
Everything — entities, forms, screens, reports, workflows, rules, integrations — is
stored as metadata in PostgreSQL and interpreted at runtime by a Rust engine.

> **Status:** Architecture & planning. No code yet.

## Documents

- [`PLAN.md`](./PLAN.md) — the full architecture & build plan (v0.3).
  Key sections:
  - §5.1 / §5.7 — storage model & referential integrity (real table per entity + native Postgres FKs)
  - §5.8 — draft → validate → publish → activate lifecycle
  - §5.9 — concurrency & transactional semantics (OCC + transactional outbox)
  - §5.10 — real-time UI channel (SSE over the event log)
  - §5.11 — multi-grained authorization (tenant / object / record / field / action / value)
  - §5.13 — bulk data import/export (record level)
  - §5.14 — attachments & blob storage
  - §5.15 — retention of high-volume append-only tables (audit/event/outbox)
  - §5.16 — threat model: untrusted metadata
  - §5.17 — reporting query model & security (structured metadata, runner-context AuthZ by construction)
- [`docs/REVIEW.md`](./docs/REVIEW.md) — critical review of the plan (C1–C6 resolved in v0.2; further refinements as ADRs 0011–0016 in v0.3; reasoning trail).
- [`docs/ri-strategies.md`](./docs/ri-strategies.md) — how major platforms handle referential integrity.
- [`docs/adr/`](./docs/adr/) — Architecture Decision Records (16 ADRs: storage/RI, lifecycle + publish/migration execution, concurrency + workflow chaining, real-time, multi-grained authz + sharing materialization + value-constraint composition, reporting query model, deletion & restoration, job queue, meta-model, frontend, GraphQL).

## Roadmap (summary)

Phased from foundation → metadata engine → dynamic data → security → rules →
workflow → UI → reporting → Studio → integrations → bulk data & attachments → hardening.
See §9 of `PLAN.md` (MVP milestone lands ~week 26).
