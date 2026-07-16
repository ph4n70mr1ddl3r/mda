# Phase 2 — Dynamic Data Layer (status & handoff)

**Status: core complete & verified.** Implements PLAN §9 Phase 2's primary
deliverable: *create/read/update records via REST over real, metadata-generated
`biz.<table>` tables with native FK-enforced relationships, and validated
schema migration on publish.* Exercised by 3 E2E tests + a live `curl` run.

## What was built

**`mda-data`** (PLAN §5.1 / §5.7 / §5.9):
- **DDL engine** (`ddl.rs`): `biz.<table>` generation, realizing Pattern B —
  - **reference fields → real typed columns with native `FOREIGN KEY`s**
    (`master_detail` NOT NULL + CASCADE; `lookup` nullable + `on_delete`);
  - **unique/indexed scalar fields → GENERATED columns** derived from the
    `attributes JSONB` payload (single source of truth — no dual-write; the
    generated column carries `UNIQUE`/index);
  - plain scalars live in `attributes JSONB`;
  - core columns (`id` ULID/uuid, `tenant_id`, `owner_id`, `state`,
    `version`, timestamps) + `attributes JSONB`.
- **CRUD + list** (`crud.rs`): generic create/read/update/delete/list over any
  active entity via **parameterized** dynamic SQL (values bound; only validated
  identifiers interpolated — §5.16); `to_jsonb(t.*)` reads reconstructed into
  records; **OCC** (`version` + `If-Match` → 409); declarative validation
  (required/type coercion) + literal defaults; **`auto_number`** via a
  per-(tenant,entity,field) gapless sequence (`meta.md_sequence`).
- **list** with `?filter=field:op:value` (eq/ne/gt/.../like), `?sort=±field`,
  `page`/`page_size` (offset paging; cursor paging is a follow-up).

**Publish integration** (Studio): the same publish transaction now also
materializes/evolves `biz` tables (all PG DDL is transactional):
- new entity → `CREATE TABLE`; new field on an existing entity → `ALTER ADD
  COLUMN`; new relationship → `ALTER ADD` FK column + constraint + index;
- **two-phase retire** (Phase 2): a removed entity/field → `status='retired'`
  + `md_retirement` (purge after 14-day grace); live data is kept (runtime
  excludes retired entities/fields);
- **transforms (type change / rename) are rejected** with a clear 422 — the
  ADR-0011 expand/contract staged migration is a follow-up.

**Runtime API** (`mda-api/data.rs`, §7):
- `GET    /api/data/:entity` — list (filter/sort/paging)
- `POST   /api/data/:entity` — create
- `GET    /api/data/:entity/:id` — read
- `PATCH  /api/data/:entity/:id` — update (`If-Match: <version>` → 409 on conflict)
- `DELETE /api/data/:entity/:id` — hard delete

## Verification (all green)

- `cargo fmt --check` · `cargo clippy -D warnings`
- unit: `mda-core` (4), `mda-meta` draft (7), `mda-data` ddl (1)
- `cargo test --test integration` (schema), `--test studio` (3), `--test data` (3)
- `--test data` covers: publish creates the biz table; create/read/**OCC (409 on
  wrong version, version bump)**; list with filter/sort; **native FK** (valid ref
  ok, dangling ref rejected); add-field **migration**; **retire** (entity → 404
  at runtime); transform rejected.
- live `curl`: publish → create → read → list(filter) → unique-enforced.

## Phase-2 decisions (made autonomously, per the plan)

- **GENERATED columns** for unique/indexed fields (JSONB = source of truth) — the
  cleanest §5.1 realization; avoids per-column typed binding. CRUD writes only
  `attributes` + FK columns.
- **Additive + retire in the publish transaction**; transforms deferred.
- **`biz.<table>` is global** (one table per entity, `tenant_id` column) — so
  entity `table_name`s must be unique per database (enforced nowhere yet; a
  global uniqueness check at validate is a follow-up). `md_entity.name` stays
  tenant-scoped (runtime addresses entities by name within a tenant).
- `owner_id` left NULL (set from auth in Phase 3).

## Deferred (clearly, not lost) — the remaining Phase-2 items

- **GraphQL prototype (ADR-0010)** — a dynamic read schema over the data layer.
  Not MVP-blocking; the next focused increment. REST already covers MVP CRUD.
- **ADR-0011 expand/contract** for transforms (type change/rename) + large
  backfills — currently rejected with 422.
- **Purge job** (apalis) to drop retired columns/tables past grace — retire is
  implemented; the scheduled drop is not yet wired.
- **Archive/restore (ADR-0006/0015)** — delete is hard-delete today; the
  `biz_archive` trigger + batch restore is a follow-up.
- **Refinements:** constraint-violation errors → cleaner 409/422 (currently a DB
  error surfaces as 500, e.g. duplicate unique value); cursor-based pagination;
  global `table_name` uniqueness at validate; `enum` CHECK constraints.

## How to try it

```bash
docker compose up -d postgres redis
DATABASE_URL=postgres://mda:mda@127.0.0.1:5433/mda?sslmode=disable cargo run

# publish Customer (see docs/PHASE1.md for the draft→publish flow), then:
curl -X POST localhost:8080/api/data/Customer -H "x-tenant-id: $TENANT" \
     -H "content-type: application/json" -d '{"name":"Acme","email":"a@b.c"}'
curl "localhost:8080/api/data/Customer?filter=name:eq:Acme" -H "x-tenant-id: $TENANT"
```
