# Phase 0 — Foundation (status & handoff)

**Status: complete & verified.** Implements PLAN §9 Phase 0 and the ADR-0009
frontend spike. The Phase 0 deliverable — *`cargo run` boots, `/health`
responds, migrations run* — is met and exercised against a live Postgres 16.

## What was built

**Rust workspace** (PLAN §6 layout, started small per the granularity note):
- `mda-core` — `Error`/`Result` + `Id` (ULID, serialized as the 26-char string;
  converts to native `uuid::Uuid` for storage, per ADR-0001/§5.1). 4 unit tests.
- `mda-meta` — typed structs (`Module`, `Entity`, `Field`, `Relationship`)
  matching the initial migration. Loader + `MetadataCache` land in Phase 1.
- `mda-data` — placeholder crate (DDL engine + CRUD arrive in Phase 2).
- `mda-api` — Axum edge; `AppState { pool }`, `router()`, `GET /health`
  (pings `SELECT 1`, returns 200/503 + JSON).
- `mda-server` — env-driven `Settings`, `tracing` init, `PgPool`, embedded
  `sqlx::migrate!`, `axum::serve` with graceful shutdown (Ctrl-C / SIGTERM).

**Migrations** (`migrations/20260101000001_init_meta.sql`): the Phase 0 *meta
schema skeleton* — `meta.md_module`, `md_entity`, `md_field`, `md_relationship`
(§4.1) and the lifecycle tables `md_active_version`, `md_draft` (incl. the
one-`publishing`-draft-per-tenant partial unique index, ADR-0011),
`md_snapshot`, `md_migration_log`, `md_retirement` (§4.8). `tenant_id` is
present on all tables; the FK to a real tenant + RLS arrive in Phase 3 (§5.4).

**Dev environment & CI:**
- `docker-compose.yml` — Postgres 16 + Redis 7 (+ optional `app` profile).
- `Dockerfile` — multi-stage Rust build.
- `.github/workflows/ci.yml` — fmt + clippy (`-D warnings`) + unit tests + an
  integration test against a Postgres service.
- `.env.example`, `.gitignore` (Cargo.lock is now committed — binary workspace).

**Frontend spike (ADR-0009):** `web/spike-leptos/` (Leptos 0.6 CSR + Trunk) and
`web/spike-react/` (React 18 + TS + Vite) — both a metadata-driven form
renderer, both **build-verified**. See `web/README.md`. The spike produces
evidence; the Leptos-vs-React decision is a human call (record as an ADR).

## Verification (all green)

- `cargo fmt --all -- --check` — clean
- `cargo clippy --all-targets --all-features -- -D warnings` — clean
- `cargo test --lib --bins` + `mda-core` unit tests — 4 passed
- `cargo test --test integration` (against live Postgres) — migrations apply,
  all 9 meta tables present, bootstrap `md_active_version` row exists
- `cargo run` + `curl /health` → `200 {"status":"ok","database":"up","version":"0.1.0"}`
- `web/spike-react`: `npm run build` ✓  ·  `web/spike-leptos`: `trunk build` ✓

## How to run

```bash
docker compose up -d postgres redis
DATABASE_URL=postgres://mda:mda@127.0.0.1:5433/mda?sslmode=disable cargo run
curl localhost:8080/health
```

## Notes / gotchas

- **Dev Postgres is on port 5433**, not 5432. A host-installed Postgres on
  `127.0.0.1:5432` was intercepting connections during development; moving the
  container to 5433 removed the ambiguity. CI runners are clean and keep 5432.
- `mda-server` config is a Phase-0 env-based stand-in for config-rs/figment
  (PLAN §3); call sites (`Settings::load()`) won't change when a file-aware
  loader is layered in.
- `sqlx` is used with **runtime** queries for now (no compile-time DB needed to
  build); `cargo sqlx prepare` offline data can be layered once there are typed
  metadata queries in Phase 1.

## Next: Phase 1 — Metadata Engine (PLAN §9, Weeks 3–6)

- `md_*` loader + `moka` cache keyed by `(tenant_id, entity_id)` + LISTEN/NOTIFY
  invalidation + version-stamp poll fallback (§5.3).
- Draft → publish lifecycle (§5.8): **additive ops only** in Phase 1
  (transforming/destructive come in Phase 2).
- Studio API: draft branch / edit (`If-Match` etag) / validate / publish;
  JSON export/import.
- First E2E test scaffold (define-model → use-it) is in place to extend.
