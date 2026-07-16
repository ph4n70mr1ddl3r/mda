# MDA — Model-Driven Architecture Enterprise Platform

A declarative, data-driven, model-driven **no-code enterprise system** built in Rust.
Everything — entities, forms, screens, reports, workflows, rules, integrations — is
stored as metadata in PostgreSQL and interpreted at runtime by a Rust engine.

> **Status:** Architecture & planning. No code yet.

## Documents

- [`PLAN.md`](./PLAN.md) — the full architecture & build plan (v0.2).
  Key sections:
  - §5.1 / §5.7 — storage model & referential integrity (real table per entity + native Postgres FKs)
  - §5.8 — draft → validate → publish → activate lifecycle
  - §5.9 — concurrency & transactional semantics (OCC + transactional outbox)
  - §5.10 — real-time UI channel (SSE over the event log)
  - §5.11 — multi-grained authorization (tenant / object / record / field / action / value)
  - §5.13 — bulk data import/export (record level)
  - §5.14 — attachments & blob storage
- [`REVIEW.md`](./REVIEW.md) — critical review of the plan (gaps resolved in v0.2).
- [`docs/ri-strategies.md`](./docs/ri-strategies.md) — how major platforms handle referential integrity.
- [`docs/adr/`](./docs/adr/) — Architecture Decision Records (storage, lifecycle, concurrency, real-time, authz, deletion, job queue, meta-model, frontend).

## Roadmap (summary)

Phased from foundation → metadata engine → dynamic data → security → rules →
workflow → UI → reporting → Studio → integrations → hardening. See §9 of `PLAN.md`.
