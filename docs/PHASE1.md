# Phase 1 — Metadata Engine (status & handoff)

**Status: complete & verified.** Implements PLAN §9 Phase 1 and the §5.3 / §5.8
design. The Phase 1 deliverable — *branch a draft, add a "Customer" entity,
validate, publish; the cache reflects the new active model* — is met and
exercised by an in-process E2E test and a live `curl` run.

## What was built

**`mda-meta`** (loader + cache + lifecycle logic):
- `EntityDefinition` (entity + fields + relationships) and `DraftModel`
  (`modules`/`entities` with nested `fields`/`relationships`) — the JSONB
  document a draft holds and the shape JSON bundles use for export/import.
- **Pure additive-only diff/validate** (`draft::diff`) — unit-tested (7 tests):
  anything active must remain *unchanged* in the draft (removal/transform are
  flagged as Phase-2 violations); additions are checked for unique names/ids,
  known field types, valid table names, and resolved relationship targets.
- **Loaders** (`load_active_model`, `load_entity_definition`, `active_version`,
  `entity_ids_for_tenant`) over the `meta.md_*` tables.
- **`MetadataCache`** — `moka`, keyed by `(tenant_id, entity_id)` so it cannot
  leak across tenants; read-through on miss. Invalidation: `LISTEN meta_changed`
  fast path + a 30 s **version-stamp poll** self-healing fallback (§5.3).
  `spawn_listen` / `spawn_poll` are started by `mda-server`.

**`mda-api`** (Studio API, PLAN §7):
- `POST /api/studio/drafts` — branch from active.
- `GET  /api/studio/drafts/:id` — read draft.
- `PUT  /api/studio/drafts/:id/model` — replace draft model under `If-Match`
  etag → **409 on conflict**, 422 if `If-Match` missing.
- `POST /api/studio/drafts/:id/validate` — dry-run diff report.
- `POST /api/studio/drafts/:id/publish` — additive-only publish in one txn:
  archive prior model to `md_snapshot`, INSERT new modules/entities/fields/
  relationships, bump `md_active_version`, mark draft published, then
  `pg_notify('meta_changed')` + drop the cache.
- `GET /api/studio/model` and `/api/studio/export` — active model as JSON.
- `POST /api/studio/import` — a JSON bundle becomes a new draft.
- `GET /api/studio/snapshots` — publish history.
- `GET /api/studio/entities/:id` — entity definition **through the cache**.
- `TenantId` extractor (`X-Tenant-Id`, defaults to the bootstrap tenant) — a
  Phase-1 stand-in; Phase 3 derives the tenant from real auth.
- `ApiError` maps `mda_core::Error` → HTTP (422 invalid / 404 / 409 conflict /
  500); `Conflict` added to the core error for OCC-style 409s.

## Verification (all green)

- `cargo fmt --check` · `cargo clippy -D warnings`
- unit tests: `mda-core` (4), `mda-meta` draft (7)
- `cargo test --test integration` (schema) — passes
- `cargo test --test studio` — 3 E2E tests:
  - branch → validate → publish → active model + **cache** reflect Customer
  - additive-only rejects a removal (validate `valid:false`, publish 422)
  - `If-Match` conflict → 409; missing `If-Match` → 422
- live `curl` through the running server: the full flow, with the `LISTEN
  meta_changed` invalidator active.

## Phase-1 decisions (made autonomously, per the plan)

- **Additive-only publish in a single transaction** — §5.8 classifies additive
  ops as "apply immediately; no data risk," so Phase 1 doesn't need the ADR-0011
  staged/cutover machinery (that arrives with transforms in Phase 2).
- **Document-style editing** — `PUT /drafts/:id/model` replaces the whole draft
  model (matches "Studio mutates the draft JSONB"). The §7 sub-resource
  endpoints (`POST …/entities`) are omitted as convenience; the document PUT
  covers "define entities/fields."
- **`biz.*` table generation deferred to Phase 2** — Phase 1 publish updates the
  `meta` model + version only (no business-data tables yet).
- **Single editor via etag** — optimistic `version_etag` is the correctness
  gate; the explicit `/checkout` UX lock is deferred (Phase 1 doesn't need it).

## How to run / try it

```bash
docker compose up -d postgres redis
DATABASE_URL=postgres://mda:mda@127.0.0.1:5433/mda?sslmode=disable cargo run

# in another shell
TENANT=00000000-0000-0000-0000-000000000000
curl -s localhost:8080/health
curl -s -X POST localhost:8080/api/studio/drafts -H "x-tenant-id: $TENANT" \
     -H "content-type: application/json" -d '{"name":"v1"}'
# … PUT /drafts/:id/model (with If-Match), POST /validate, POST /publish
```

## Next: Phase 2 — Dynamic Data Layer (PLAN §9, Weeks 6–9)

- `mda-data`: **DDL/migration engine** — publish generates `biz.<table>`
  (core + hoisted relational/scalar columns + native FKs, §5.1/§5.7) and
  classifies ops additive/transforming/destructive (§5.8).
- Transforming ops (data casts) + two-phase destructive (retire → purge).
- Generic CRUD + list (filter/sort/paging over hoisted cols + JSONB).
- Reference fields → real typed columns with native FK.
- Optimistic concurrency (`version` + `If-Match` → 409), `/api/data/:entity/*`.
- GraphQL prototype (ADR-0010).
