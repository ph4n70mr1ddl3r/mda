# MDA — Model-Driven Architecture Enterprise Platform

A declarative, data-driven, model-driven **no-code enterprise system** built in Rust.
Everything — entities, forms, screens, reports, workflows, rules, integrations — is
stored as metadata in PostgreSQL and interpreted at runtime by a Rust engine.

> **Status:** Architecture & planning → **Phases 0–3 implemented** (foundation +
> metadata engine + dynamic data layer + security/auth + rules: bounded expression
> DSL, set-field rules, calculated fields).

## Quick start (Phase 0)

```bash
# 1. dependencies (Postgres 16 + Redis)
docker compose up -d postgres redis

# 2. run the server (boots, runs migrations, serves /health)
cp .env.example .env            # adjust if needed
DATABASE_URL=postgres://mda:mda@127.0.0.1:5433/mda?sslmode=disable cargo run
```

Health check:

```bash
curl localhost:8080/health
# {"status":"ok","database":"up","version":"0.1.0"}
```

Studio API (Phase 1) — branch → edit → validate → publish:

```bash
TENANT=00000000-0000-0000-0000-000000000000
curl -s -X POST localhost:8080/api/studio/drafts -H "x-tenant-id: $TENANT" \
     -H "content-type: application/json" -d '{"name":"v1"}'
# PUT  /api/studio/drafts/:id/model   (If-Match: <version_etag>)  — edit
# POST /api/studio/drafts/:id/validate                          — dry-run diff
# POST /api/studio/drafts/:id/publish                           — additive + retire
# GET  /api/studio/model                                        — active model
```

Runtime data API (Phase 2) — CRUD over generated `biz.<table>`:

```bash
TENANT=00000000-0000-0000-0000-000000000000
curl -X POST localhost:8080/api/data/Customer -H "x-tenant-id: $TENANT" \
     -H "content-type: application/json" -d '{"name":"Acme"}'
curl "localhost:8080/api/data/Customer?filter=name:eq:Acme" -H "x-tenant-id: $TENANT"
# PATCH /api/data/:entity/:id  (If-Match: <version>)  — OCC update (409 on conflict)
```

> Dev Postgres is published on **5433** (not 5432) to avoid colliding with a
> host-installed Postgres during local development.

Tests:

```bash
cargo test --lib --bins                     # unit tests (no DB needed)
DATABASE_URL=postgres://mda:mda@127.0.0.1:5433/mda?sslmode=disable \
  cargo test --test integration             # applies migrations + checks schema
```

Frontend spike (ADR-0009): see [`web/README.md`](./web/README.md).

## Documents

- [`PLAN.md`](./PLAN.md) — the full architecture & build plan (v0.4).
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
  - §5.18–5.22 — platform capabilities: notifications & messaging, templating, secrets management, event/webhook contract, integration architecture (hub model)
  - §14 — tracked, not yet designed (platform gaps)
- [`docs/REVIEW.md`](./docs/REVIEW.md) — critical review of the plan (C1–C6 resolved; further refinements as ADRs 0011–0017; reasoning trail).
- [`docs/ri-strategies.md`](./docs/ri-strategies.md) — how major platforms handle referential integrity.
- [`docs/adr/`](./docs/adr/) — Architecture Decision Records (17 ADRs: storage/RI, lifecycle + publish/migration execution, concurrency + workflow chaining, real-time, multi-grained authz + sharing materialization + value-constraint composition, reporting query model, deletion & restoration, rollup summaries, job queue, meta-model, frontend, GraphQL).
- [`docs/PHASE0.md`](./docs/PHASE0.md) · … · [`docs/PHASE5.md`](./docs/PHASE5.md) — phase status & handoffs.

## Roadmap (summary)

Phased from foundation → metadata engine → dynamic data → security → rules →
workflow → UI → reporting → Studio → integrations → bulk data & attachments → hardening.
See §9 of `PLAN.md` (MVP milestone lands ~week 26).

## Layout

```
crates/        Rust workspace: mda-core, mda-meta, mda-data, mda-api, mda-server
migrations/    SQLx migrations (Phase 0: meta schema skeleton)
web/           Phase 0 frontend spike (Leptos + React) — throwaway
docker/ (in repo root: docker-compose.yml, Dockerfile)
.github/       CI
```
