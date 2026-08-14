# MDA — Model-Driven Architecture Enterprise Platform

A declarative, data-driven, model-driven **no-code enterprise system** built in Rust.
Everything — entities, forms, screens, reports, workflows, rules, integrations — is
stored as metadata in PostgreSQL and interpreted at runtime by a Rust engine.

> **Status:** Phases 0–10 implemented — including the **Phase 8 Studio UI**:
> the Leptos Runtime UI grows an admin-gated Studio (model designer over the
> draft → validate → publish lifecycle, page designers, report designer,
> rule editor + workflow designer, security admin console, import/export),
> backed by new **rules + workflows authoring APIs** (`/api/rules`,
> `/api/workflows`) and completed draft management (`GET /api/studio/drafts`,
> discard). Phase 6 Runtime UI (Leptos)
> **rendering from metadata** (navigation / view / form / dashboard definitions),
> plus the full §5.18–5.22 platform-capability cluster and a first-class GraphQL
> runtime API (ADR-0010).
> Server: auth, CRUD, security, rules, workflows, reporting, bulk (CSV+JSON import with dry-run / create·update·upsert by key / on_error abort+continue), attachments,
> notifications, sharing — now the **complete §5.11.3 composition** (ADR-0013
> closed, ADR-0026): manual shares, **criteria-based sharing rules** (epoch-
> gated materialization + synchronous per-record recompute), team-OWD record
> visibility, team hierarchy (ADR-0025 `sec_team.parent_id`), and **role
> hierarchy** (live, read-only "see records below me"). Platform: secrets, templating, multi-channel
> notifications + digest (with record-reader recipient resolution +
> FLS-under-recipient rendering + SMTP send), signed webhook contract + inbound
> verification, hub-model integration (connectors / flows / external-ID registry,
> with `field_level_sor` conflict policy, debatching, per-flow running user, and
> cron-scheduled pulls), GraphQL (reads **and** mutations, hot-invalidated on
> publish — ADR-0024), cron-driven **scheduled-job management** (§14, including
> scheduled integration pulls), tenant configuration **export + import** (§14
> backup/restore), **mass actions** (bulk update/delete by filter — ADR-0021),
> **API versioning & deprecation** with `Sunset`/`Deprecation` headers
> (ADR-0022), and **metadata/UI i18n** (`md_translation`, best-match locale —
> ADR-0023). **Team hierarchy** (ancestor-team visibility — ADR-0025) and the
> **role hierarchy / sharing rules** (ADR-0026) ship with a superuser **admin
> security API** (`/api/admin/{teams,roles,owd,users,share-rules}` +
> `/api/admin/roles/:id/parents`) that makes the whole security graph operable.
> Reporting is complete: **authoring CRUD**, **reference-traversal joins**
> (`customer_id.name` over hoisted FKs), and **CSV/HTML/XLSX/PDF export** —
> with scheduled delivery (`report.completed` notification).
> Frontend (WASM): login, navigation shell, view-driven grids,
> form-definition-driven editors (incl. reference pickers), dashboards, and the
> real-time conflict banner — plus the **Studio** (Phase 8): model designer
> (entities/fields/references + draft → validate → publish), page designers
> (forms/views/dashboards/navigation), report designer, rule editor +
> workflow designer, security admin console, and model import/export.
>
> **Platform surfaces (ADR-0018):** record/field history + as-of
> (`/api/data/:entity/:id/{history,as-of}`), a tenant observability console
> (`/api/observability/{events,outbox,migrations,audit}`), and a stable
> error-code taxonomy (`mda.<kind>`) on every error response.

## Quick start (Phase 0)

```bash
# 1. dependencies (Postgres 16 + Redis) — Docker OR Podman:
docker compose up -d postgres redis
# podman compose up -d postgres redis   # Podman 4+ works with the same file

cp .env.example .env            # adjust if needed

# 2. run the server (boots, runs migrations, serves /health):
DATABASE_URL=postgres://mda:mda@127.0.0.1:5433/mda?sslmode=disable cargo run
# or:  make run-dev   (starts the deps + runs the server)
```

Health check:

```bash
curl localhost:8080/health
# {"status":"ok","database":"up","version":"0.1.0"}
```

Login is **tenant-scoped** (the client names the tenant; this is what lets the
DB enforce `sec_user` row-level security — login sets the tenant context before
the user lookup). The tenant may be a slug or a UUID; the bootstrap admin's
tenant has slug `default`:

```bash
curl -X POST localhost:8080/api/auth/login \
     -H 'content-type: application/json' \
     -d '{"tenant":"default","email":"admin@mda.local","password":"admin123"}'
# {"access_token":"…","refresh_token":"…","token_type":"Bearer"}
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
# Mass actions (ADR-0021) — bulk update/delete by filter, reusing the write pipeline:
#   POST /api/data/:entity/mass-update   {"filter":["tier:eq:Bronze"],"set":{"tier":"Silver"},"dry_run":false}
#   POST /api/data/:entity/mass-delete   {"filter":["tier:eq:Bronze"]}
# Bulk import/export (§5.13) — mapped, validated, safe record import (CSV or JSON):
#   POST /api/impex/:entity/import?mode=upsert&key=email&dry_run=true&on_error=abort  (text/csv body)
#   GET  /api/impex/:entity/export  (-> text/csv; respects field-level read security)
# Manual record sharing (§5.11) — grant / list / revoke (owner-or-superuser):
#   POST   /api/shares/:entity/:id              {"principal_id":"…","access":"read|write"}
#   GET    /api/shares/:entity/:id              -> [{principal_id, access, name, email, created_at}]
#   DELETE /api/shares/:entity/:id/:principal_id -> 204
# Attachments (§5.14) — sha256-checksummed, dedup-by-content, refcount-cleaned:
#   POST   /api/attachments         (raw bytes) -> {id, filename, mime, size, checksum}
#   GET    /api/attachments/:id                  -> bytes (owner/superuser)
#   DELETE /api/attachments/:id                  -> 204 (reclaims bytes only when last ref)

# UI definitions (Phase 6) — renderable metadata for the Runtime UI (FLS-projected):
#   GET  /api/forms/:entity[?name=]   sections+fields (widgets, options, reference targets)
#   GET  /api/views/:entity[?name=]   grid columns + default filters/sort/page size
#   GET  /api/dashboards[/:id]        definitions; :id RUNS each report under the caller
#   GET  /api/navigation              the caller's permission-filtered menu
#   (POST/PATCH/DELETE on the same paths author the definitions)

# Reports (§5.17) — author, run, export (csv|html|xlsx|pdf); fields may traverse
# references (customer_id.name) via real LEFT JOINs over the hoisted FK columns:
#   POST/GET /api/reports[/:id]       CRUD on meta.md_report
#   GET  /api/reports/:id/run         run under the caller's full security
#   GET  /api/reports/:id/export?format=pdf

# Sharing rules + role hierarchy (ADR-0013 closed / ADR-0026):
#   POST/GET/PATCH/DELETE /api/admin/share-rules[/:id]       criteria rules (user|team principal)
#   POST /api/admin/share-rules/:id/recompute?from=&limit=   resumable re-materialization
#   POST/GET/DELETE  /api/admin/roles/:id/parents[/:parent]  role hierarchy (read-only visibility)
```

Platform surfaces (ADR-0018) — record history & as-of, and the tenant
observability console (superuser-gated). Every error carries a stable
`code` (SDK/i18n key) in addition to `message`:

```bash
# timeline of changes for a record (per-field diffs, FLS-projected)
curl "localhost:8080/api/data/Customer/$ID/history" -H "x-tenant-id: $TENANT"
# reconstruct the record at version 1 (or ?at=2025-07-18T12:00:00Z)
curl "localhost:8080/api/data/Customer/$ID/as-of?version=1" -H "x-tenant-id: $TENANT"
# operator console: domain events, delivery queue, publish log, audit trail
curl localhost:8080/api/observability/events    -H "Authorization: Bearer $ADMIN_JWT"
curl localhost:8080/api/observability/outbox     -H "Authorization: Bearer $ADMIN_JWT"
curl localhost:8080/api/observability/migrations -H "Authorization: Bearer $ADMIN_JWT"
curl localhost:8080/api/observability/audit      -H "Authorization: Bearer $ADMIN_JWT"
# {"code":"mda.conflict","status":409,"error":"conflict","message":"…"}
```

Platform capabilities (§5.18–5.22) + GraphQL (ADR-0010) — all driven by metadata,
AuthZ-enforced, sharing REST's service layer:

```bash
# GraphQL — schema generated from the active model; reads + nested traversal
# + create/update/delete mutations (sharing REST's write service), all with
# object/field/record security enforced per field.
curl -X POST localhost:8080/api/graphql -H "Authorization: Bearer $JWT" \
     -H 'content-type: application/json' \
     -d '{"query":"{ customer(id:\"…\") { name customer { name } } }"}
# mutation createCustomer(input: {name: "Acme"}) { id name version } }'
curl -X POST localhost:8080/api/graphql -H "Authorization: Bearer $JWT" \
     -H 'content-type: application/json' \
     -d '{"query":"mutation { createCustomer(input: {name:\"Acme\"}) { id name version } }"}'

# Secrets (§5.20) — only the reference is stored; values resolve server-side and
# are never returned by any API.
curl -X POST localhost:8080/api/secrets -H "Authorization: Bearer $JWT" \
     -H 'content-type: application/json' -d '{"name":"smtp_password","ref":"MDA_SMTP"}'

# Templating (§5.19) — sandboxed DSL; render under the caller's FLS.
# Localizes via the i18n bundle: {{ i18n.email.subject }} (ADR-0023).
curl -X POST localhost:8080/api/templates/welcome/render?entity=Customer&id=$ID&locale=fr \
     -H "Authorization: Bearer $JWT" -H 'content-type: application/json' -d '{}'

# Notifications (§5.18) — types, per-user preferences, multi-channel fan-out +
# digest. Recipients may be explicit or resolved as “everyone who can read this
# record” (recipient_strategy=record_readers); email bodies are FLS-projected
# per recipient and delivered via the pluggable SMTP MailSender.
curl -X POST localhost:8080/api/notifications/dispatch -H "Authorization: Bearer $JWT" \
     -H 'content-type: application/json' \
     -d '{"type_key":"invoice.overdue","recipients":["$USER_ID"],"context":{"record":{"name":"Acme"}}}'
curl -X POST localhost:8080/api/notifications/dispatch -H "Authorization: Bearer $JWT" \
     -H 'content-type: application/json' \
     -d '{"type_key":"record.changed","recipient_strategy":"record_readers","entity":"Customer","record_id":"$ID","context":{"record":{"name":"Acme"}}}'

# Webhooks (§5.21) — versioned HMAC-signed envelope; inbound receiver verifies + dedupes.
curl -X POST localhost:8080/api/webhooks -H "Authorization: Bearer $JWT" \
     -H 'content-type: application/json' \
     -d '{"name":"sync","url":"https://ext/hook","event_types":["record.created"],"secret_ref":"wh_secret"}'

# Integration (§5.22) — hub model: inbound materializes into biz.*, keyed by
# external id. Conflict policies: last_write_wins | manual | field_level_sor;
# a `debatch` flow step fans one payload into many records.
curl -X POST localhost:8080/api/flows/$FLOW_ID/run -H "Authorization: Bearer $JWT" \
     -H 'content-type: application/json' \
     -d '{"payload":{"external_id":"A1","name":"Acme","tier":"Gold"}}'
curl "localhost:8080/api/external-ids/Customer/A1?system=acme" -H "Authorization: Bearer $JWT"

# Scheduled jobs (§14) — cron-driven modeler schedules with next-run/last-run/
# failure state + per-run history. `report` runs a saved report under the
# schedule's running user; `integration` pulls an inbound flow from its connector
# on cadence (scheduled sync); `custom` is an extensibility hook.
curl -X POST localhost:8080/api/schedules -H "Authorization: Bearer $JWT" \
     -H 'content-type: application/json' \
     -d '{"name":"nightly","kind":"report","target_id":"'$REP_ID'","cron":"0 0 * * * *"}'
curl localhost:8080/api/schedules/$SCHED_ID/runs -H "Authorization: Bearer $JWT"

# i18n (ADR-0023) — metadata/UI string translations, best-match locale
# (exact → language prefix → default ''). Ships in the tenant config export.
curl -X POST localhost:8080/api/translations -H "Authorization: Bearer $JWT" \
     -H 'content-type: application/json' \
     -d '{"locale":"fr","namespace":"ui","key":"greeting","value":"Bonjour"}'
curl localhost:8080/api/i18n/fr -H "Authorization: Bearer $JWT"   # resolved bundle

# API versioning (ADR-0022) — pin a major; deprecated majors advertise Sunset.
curl localhost:8080/health -H "X-API-Version: 1"   # → MDA-API-Version: 1

# Tenant config export/import (§14 backup/restore) — a portable JSON snapshot of
# the tenant's model + reports + schedules + security graph + integrations.
# Import merges by natural key (idempotent; FKs remapped) into the caller's
# tenant and stages the model as a Studio draft.
curl localhost:8080/api/tenants/export -H "Authorization: Bearer $JWT" > tenant-backup.json
curl -X POST localhost:8080/api/tenants/import -H "Authorization: Bearer $JWT" \
     -H 'content-type: application/json' --data @tenant-backup.json

# Admin security API (ADR-0025) — superuser-only management of the security
# graph: teams (incl. the parent/sub-team hierarchy), roles, object/field
# permissions, org-wide defaults, and users. Under team-OWD a member of an
# ancestor (manager) team reads records owned by any descendant team.
curl -X POST localhost:8080/api/admin/teams -H "Authorization: Bearer $JWT" \
     -H 'content-type: application/json' -d '{"name":"Eng"}'
curl -X POST localhost:8080/api/admin/teams -H "Authorization: Bearer $JWT" \
     -H 'content-type: application/json' -d '{"name":"Eng-Platform","parent_id":"'$ENG_ID'"}'
curl -X PUT  localhost:8080/api/admin/owd/Customer -H "Authorization: Bearer $JWT" \
     -H 'content-type: application/json' -d '{"default_access":"team"}'
curl -X POST localhost:8080/api/admin/users -H "Authorization: Bearer $JWT" \
     -H 'content-type: application/json' -d '{"email":"a@x","name":"A","password":"...","team_id":"'$ENG_ID'"}'
curl -X POST localhost:8080/api/admin/roles -H "Authorization: Bearer $JWT" \
     -H 'content-type: application/json' -d '{"name":"Rep"}'
curl -X POST localhost:8080/api/admin/roles/$ROLE_ID/permissions -H "Authorization: Bearer $JWT" \
     -H 'content-type: application/json' -d '{"entity":"Customer","verb":"read"}'
curl -X POST localhost:8080/api/admin/users/$USER_ID/roles -H "Authorization: Bearer $JWT" \
     -H 'content-type: application/json' -d '{"role_id":"'$ROLE_ID'"}'
```

> Dev Postgres is published on **5433** (not 5432) to avoid colliding with a
> host-installed Postgres during local development.

Tests:

```bash
# unit + doc tests (no DB needed)
cargo test --lib --bins --doc

# DB-backed suites — the real verification (CRUD, publish, RLS, SSE,
# archive/restore, sharing, workflow, reporting). Each test gets its own fresh
# database, so they run fully in parallel:
DATABASE_URL=postgres://mda:mda@127.0.0.1:5433/mda?sslmode=disable \
  cargo test --test data --test studio --test integration
# or:  make test   (unit + DB-backed)
#
# Platform-capability + GraphQL + scheduler + tenant suites (§5.18–5.22, ADR-0010, §14):
#   cargo test --test secrets --test templates --test notifications \
#              --test webhooks --test integration_flows --test graphql --test scheduler --test tenants --test admin
```

The `data`/`studio` suites connect the app as the non-superuser `mda_app` role
(created by the RLS migration) so `biz.*` row-level security actually engages;
superusers bypass RLS, so the owner role can't be the one under test.

## Deployment

- **Dev:** `docker compose` / `podman compose` (see Quick start).
- **Staging (prod-like):** `make up-staging` →
  `docker compose -f docker-compose.yml -f compose.staging.yml --profile app up -d`
  (json logs, restart policies, a memory ceiling; still serves as `mda_app`).
- **Production:** Podman + **Quadlet** (systemd-managed containers) — see
  [`deploy/quadlet/README.md`](./deploy/quadlet/README.md). `make quadlet-install`
  lays down the `.container`/`.network`/`.volume` units; secrets live in
  `/etc/mda/mda-app.env` (`MDA_APP_DATABASE_URL`, `MDA_JWT_SECRET`).

`make` targets take `CTN=podman` to switch runtimes, e.g. `make up-staging CTN=podman`.

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
  - §5.18–5.22 — platform capabilities: notifications & messaging, templating, secrets management, event/webhook contract, integration architecture (hub model), **scheduled-job management (§14)**
  - §14 — tracked, not yet designed (platform gaps)
- [`docs/REVIEW.md`](./docs/REVIEW.md) — critical review of the plan (C1–C6 resolved; further refinements as ADRs 0011–0017; reasoning trail).
- [`docs/ri-strategies.md`](./docs/ri-strategies.md) — how major platforms handle referential integrity.
- [`docs/adr/`](./docs/adr/) — Architecture Decision Records (25 ADRs: storage/RI, lifecycle + publish/migration execution, concurrency + workflow chaining, real-time, multi-grained authz + sharing materialization + value-constraint composition, reporting query model, deletion & restoration, rollup summaries, job queue, meta-model, frontend, GraphQL, surfaced capabilities, scheduled jobs, platform follow-ups, mass actions, API versioning, i18n, GraphQL hot-invalidation, **team hierarchy + admin security API**).
- [`docs/PHASE0.md`](./docs/PHASE0.md) · … · [`docs/PHASE10.md`](./docs/PHASE10.md) — phase status & handoffs.
- [`docs/CAPABILITIES.md`](./docs/CAPABILITIES.md) — the §5.18–5.22 platform-capability cluster (secrets, templating, notifications, webhook contract, hub-model integration) + GraphQL (ADR-0010) status & handoff.
- [`docs/HARDENING.md`](./docs/HARDENING.md) — Phase-11 production hardening pass: release-mode E2E as the `mda_app` role, the works-as-owner bug class it exposed (schema misplacement, missing grants, runtime DDL), and the regression suites that now guard it.
- [`docs/LOADTEST.md`](./docs/LOADTEST.md) — Phase-11 load-test results (release binary, production role): throughput/latency per surface, reproduction steps, and what the numbers mean for scale-out.

## Roadmap (summary)

Phased from foundation → metadata engine → dynamic data → security → rules →
workflow → UI → reporting → Studio → integrations → bulk data & attachments → hardening.
See §9 of `PLAN.md` (MVP milestone lands ~week 26).

## Layout

```
crates/        Rust workspace: mda-core, mda-meta, mda-data, mda-expression,
               mda-security, mda-rules, mda-workflow, mda-reports,
               mda-integration, mda-api, mda-server
migrations/    SQLx migrations (Phase 0: meta schema skeleton)
web/           Phase 0 frontend spike (Leptos + React) — throwaway
docker/ (in repo root: docker-compose.yml, Dockerfile)
.github/       CI
```
