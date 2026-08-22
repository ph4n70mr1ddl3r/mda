# Production Hardening Pass (Phase 11)

> Date: 2026-08-14 · outcome: the release binary was booted against a real
> database **serving as the non-superuser `mda_app` role** (the production
> configuration: `MDA_APP_DATABASE_URL` set, `MDA_JWT_SECRET` set) and every
> platform surface was exercised end-to-end. Several bugs were invisible to
> the test suite because the tests — like `make run-dev` — run as the database
> *owner*, whose `"$user"` search_path resolves names the app role cannot see.

## The theme: works-as-owner, broken-as-`mda_app`

PostgreSQL's default `search_path` starts with `"$user"`, which expands to the
schema named after the current role. The database owner role is `mda`, and the
platform legitimately owns a *schema* named `mda` (platform trigger functions,
created by migration `20260110000001`). Consequently, **every unqualified
`CREATE TABLE` executed as role `mda` landed in schema `mda`, not `public`** —
and `mda_app` (search_path `public`) could not see it. The dev server hid all
of it because it connects as the owner.

Fixes, in migration order:

| Migration | Fixes |
|---|---|
| `20260132000001` | `sys_schedule` / `sys_schedule_run` (created unqualified by `20260126000001`) relocated `mda → public`; scheduler + `/api/schedules` were dead as `mda_app` |
| `20260133000001` | `sys_webhook_relay_cursor` became migration-owned DDL. It was `CREATE TABLE IF NOT EXISTS`-ed **at runtime on the app pool** — as `mda_app` that is `permission denied for schema public`, so the relay warn-looped and **no webhook delivery was ever enqueued** (§5.21 silently dead in every release deployment) |
| `20260134000001` | `ALTER DATABASE … SET search_path TO public` — pins resolution so the `"$user"` capture class cannot recur (it even captured *sqlx's own* `_sqlx_migrations` on databases that already had the `mda` schema, making the migration history look empty and triggering a full re-run) |
| `20260135000001` | `GRANT USAGE` + full DML + default privileges on schema `int` for `mda_app`. The RLS migration (`20260111`) granted `meta, sec, public` but `int` (webhooks, connectors, flows, external-ID registry — Phase 9) was never granted, so **every webhook/integration API call 500'd as `mda_app`** |

## Other fixes from the pass

- **500 responses no longer leak internals.** The API envelope sent the raw
  internal error text (SQL/driver messages, potentially DSNs) as `message`.
  5xx bodies now carry the stable `code` (`mda.internal_error`) and a generic
  message; details stay in the server log (`mda-api/src/error.rs`).
- **Handler panics no longer drop the connection.** A `CatchPanicLayer`
  converts them into the platform 500 error envelope (logged with the panic
  detail) instead of a reset connection (`mda-api/src/lib.rs`).
- **Env-driven header values are validated at load.** `MDA_SUNSET_DATE` /
  `MDA_DEPRECATION_LINK` values that are not valid HTTP header values
  degraded to per-request panics in the versioning middleware; they now warn
  and fall back to the defaults (`mda-api/src/versioning.rs`).
- **RFC-9110 quoted `If-Match` etags accepted.** The Studio draft API parsed
  `If-Match` as a bare UUID only; generic HTTP clients (which send
  `If-Match: "uuid"`) were rejected with 422 (`mda-api/src/studio.rs`).
- **`docker compose --profile app` / `make up-staging` boot.** The compose
  `app` service never set `MDA_JWT_SECRET`, which the release image *requires*
  — the container crash-looped. Compose now fails fast with instructions
  (`docker-compose.yml`).
- Blob download Content-Type parsing degrades to `application/octet-stream`
  instead of panicking on a malformed stored mime (`mda-api/src/blobs.rs`).

## Regression coverage added

- `tests/app_role.rs` (new suite, in `make test` + CI): after the full
  migration chain, `mda_app` must hold `USAGE` on every app schema and
  `SELECT` on every table in `public`/`meta`/`sec`/`int` — any future
  migration that forgets a grant fails the build, not staging. Also migrates
  six databases concurrently (the CI `pg_authid` role race).
- `tests/security.rs` (new suite, in `make test` + CI — the automated pen-test
  pass): SQL-injection payloads against entity paths, list filters and record
  bodies (never a 5xx, never widened results), malicious identifiers rejected
  at validate/publish, tampered/forged JWTs, malformed input, error
  content-type, and the global body limit.
- `scheduler_tables_are_visible_to_the_app_role` (in `tests/scheduler.rs`):
  the specific misplacement regression.
- `quoted_rfc_etag_is_accepted` (in `tests/studio.rs`).
- Unit tests for the error envelope (no internal leak, client errors keep
  their message) and the panic layer.

## Second pass (same day): load test + error-envelope completion

- **Load test** ([`docs/LOADTEST.md`](./LOADTEST.md)): release binary as
  `mda_app`, ~505k requests across health / authenticated list / GraphQL /
  login scenarios — zero 5xx, zero panics, server healthy at ~412 MB RSS.
  Read path ~1.4k rps/node at p99 ≈ 140 ms under c=64; login ~170 rps/node is
  the deliberate Argon2id cost.
- **Framework errors now carry the envelope too**: extractor rejections
  (malformed JSON → axum's plain-text 400) and router 404/405 fallbacks were
  the last responses bypassing the ADR-0018 JSON envelope; an innermost
  `error_envelope` middleware rewrites them (stable `code` per status, 5xx
  details still server-side only). Found by the security suite.
- `migrate::run` pre-creates the `mda_app` role race-tolerantly — recurring
  red CI: parallel test databases losing the check-then-create race on the
  cluster-global `pg_authid`.
- The runtime UI's API origin is runtime-configurable
  (`window.__MDA_API_BASE__` > build-time `MDA_API_BASE` > dev default), so
  the static nginx bundle deploys anywhere without a rebuild.

## Audited and found sound (no change needed)

- **Auth coverage**: every route's handler takes `AuthUser` (or a ticket /
  HMAC verification) except the deliberate set — `/health`, `/livez`,
  `/readyz`, `/metrics`, login/refresh/logout, the SSE ticket stream, and the
  signature-verified inbound webhook receiver.
- **SQL injection surface**: entity/field/relationship names interpolated into
  dynamic `biz.*` SQL all pass the single `is_valid_identifier` gate at
  publish time (`[a-z][a-z0-9_]*`, ≤63 chars, reserved-word and core-column
  blacklist — PLAN §5.16); user data is always bound, never interpolated.
- **Expression engine**: bounded (depth 32, step budget), division-by-zero
  and unknown-field handled as errors, not panics.
- **Release-mode secrets**: `MDA_JWT_SECRET` < 32 bytes or unset refuses to
  boot in release (dev warns and uses an insecure default).
- **CORS/security headers**: permissive only in debug builds; release is
  same-origin-only unless `MDA_CORS_ORIGINS` allow-lists; HSTS, nosniff,
  frame-deny, no-referrer set on every response.
- **Graceful shutdown** on SIGTERM/Ctrl-C; body-size limit; request-id +
  access-log + metrics middleware.

## Third pass (2026-08-15): full-code review — trust boundaries, egress, races

A four-sweep review of every crate (API authz, data/core/meta, security/rules/
workflow/integration/reports, server/deploy/UI). Findings and fixes, most
severe first; regression tests were added with each class of fix.

### Metadata validation bypass → SQL injection (high)

`mda-meta`'s `diff()` validated field/relationship identifiers only for
*brand-new entities*: a new field on an **existing** entity skipped
`is_valid_identifier` entirely — the single gate (§5.16) that makes DDL and
runtime SQL interpolation safe. A tenant superuser could publish a field named
`x' || (SELECT …) || 'y` and get a blind-exfiltration primitive inside list
filters / UPDATE SET (crossing tenant isolation on non-RLS tables).
`diff()` now validates every draft artifact whose id is not already active,
and name-uniqueness is checked against the active entity's names (a new field
can no longer shadow an active one). Defense-in-depth: the DDL layer
(`mda-data/ddl.rs`) refuses to interpolate any identifier that fails
`is_valid_identifier` (now `pub`). Regression: `rejects_invalid_field_name_on_existing_entity`,
`rejects_new_field_shadowing_active_field_name` (`mda-meta`).

### Operator surfaces were unauthenticated-but-for-a-token (high)

`/api/secrets`, `/api/webhooks`, `/api/connectors`+`/api/flows`(+`/run`),
`/api/schedules` required only a *valid login* — no role check. Any tenant
user could list secret refs, rotate/delete them, register webhooks for **all**
tenant events (`event_types: []`), aim connectors anywhere, run flows under the
system write scope, or fire another tenant's schedule (see IDOR below). All of
these surfaces are now superuser-gated (`require_admin`, same trust root as
`/api/admin`). Regression: `operator_surfaces_require_admin` (security suite).

### SSRF via connector / webhook URLs (high)

Connector `base_url` and webhook `url` accepted any string, fetched by clients
with **no timeout** and redirect-following (custom auth headers survive
cross-host redirects). Now: scheme+host validation and a private-address guard
(RFC1918, loopback, link-local incl. cloud metadata, IPv6 ULA/link-local,
checked against *every* resolved address) at registration **and** per request
(`mda-integration::net`); 15 s total / 5 s connect timeouts; redirects not
followed; fetched bodies capped at 10 MiB. On-prem internal targets are an
explicit operator opt-in (`MDA_ALLOW_PRIVATE_EGRESS=1`).

### Production bug: `flow_steps()` ran outside the tenant GUC (high)

`int.flow_step` is FORCE-RLS; the loader queried the pool directly, so as
`mda_app` it returned **nothing** — every transform/filter/value_map/debatch
step silently skipped in production (tests passed: they run as the owner).
Fixed to run under `set_tenant` like its siblings.

### Cross-tenant IDOR on schedules (high)

`sys_schedule` deliberately carries no RLS, and `GET /api/schedules/:id` +
`trigger`'s lookup (and `PATCH`'s pre-read) selected by bare `id` — the tenant
GUC is inert on that table. All three now carry an explicit `tenant_id`
predicate; wrong-tenant ids are 404.

### Other fixes

- **`restore` bypassed record-level security** — entity `create` alone could
  resurrect (and read) any archived tenant record. The caller's write-scope
  predicate is now applied to the archive row (`mda-data::restore`).
- **Bootstrap admin password** — release builds refuse to boot on the
  documented dev default (`admin123`); `MDA_BOOTSTRAP_PASSWORD` (≥ 12 chars)
  is required, like `MDA_JWT_SECRET`. Wired into compose + the quadlet env
  example.
- **Admin password reset now revokes the user's sessions** in the same
  transaction (a forced reset is a compromise response).
- **Login user-enumeration timing** — the unknown-user path burns the Argon2
  work against a fixed dummy hash, matching the bad-password path's timing.
- **`X-Forwarded-For` is trusted only under `MDA_TRUST_PROXY=1`** (else the
  socket peer is used) — spoofed XFFs could rotate the per-IP lockout key.
- **401s now carry the full ADR-0018 envelope** (`code`/`error`/`status`/
  `message`) — `code: mda.unauthorized` is stable for SDKs. Regression:
  `unauthorized_envelope_shape`.
- **GraphQL error scrubbing** — `Error::Internal`'s Display (SQL/driver
  detail) reached `errors[].message`; now "internal error" with the `code`
  extension preserved (REST already scrubbed).
- **Webhook replay** — `limit` is clamped (a negative limit meant *no* limit:
  one call could re-enqueue the tenant's whole event log) and replay now
  honors the subscription's `event_types`/`entity_filter` (same predicate as
  the relay).
- **Inbound webhook dedupe without `X-MDA-Event-Id`** — the dedupe key is now
  derived from `(webhook, signature timestamp, body)`, so a replay that drops
  the header is still caught inside the replay window.
- **Export truncation** — `/api/impex/:entity/export` silently emitted ≤ 200
  rows; it now pages through the filtered set (bounded by 100k rows).
- **Manual share vs rule-derived share** — a manual share can no longer
  overwrite a rule-derived row (which became unrevocable + silently reverted
  on the next recompute); the collision surfaces as a 409 naming the rule.
- **Team-hierarchy CTE** — `UNION ALL` → `UNION` (a `parent_id` cycle made the
  recursive CTE spin forever; the reimport path links parents unchecked).
- **Ordered filters on text fields** — `gt/gte/lt/lte` on non-numeric fields
  are a 422 instead of a numeric-cast 500 on non-numeric data. Regression:
  `numeric_filter_on_text_field_is_rejected_not_500`.
- **Unbounded `page`** — clamped (u64::MAX overflowed the OFFSET math).
- **Reports without a limit** — default + clamp to 10k rows (statement_timeout
  bounded time, not rows).
- **Workflow transitions** — the caller's read scope is checked before any
  superuser read, so transition errors can't disclose unreadable records'
  state.
- **Audit rows survive event-log failures** — the audit insert commits even
  when the event-log insert fails (the degraded path is counted + logged).
- **Blob dedup race** — upload/delete of the same bytes now serialize on a
  checksum-keyed advisory lock (a concurrent pair could commit a row whose
  file was just unlinked). Release builds require `MDA_BLOB_DIR` (dev default
  `/tmp/mda-blobs`, dir created 0700).
- **Runtime UI nav links** — tenant-authored nav URLs are scheme-checked
  (`http(s)`/same-app path); `javascript:` URLs render inert.
- **Dead `TenantId` extractor removed** — it trusted `X-Tenant-Id` from any
  client; unused, but a footgun for future handlers.

### Deployment surface

- Dockerfile runs as a non-root user (uid 1000) with a writable blob dir.
- The quadlet app container gains `NoNewPrivileges`, `DropCapability=all`,
  read-only rootfs + tmpfs `/tmp`, and a dedicated `mda-blobs.volume`.
- `mda_app`'s role password is rotatable via `MDA_APP_DB_PASSWORD` (applied at
  startup); compose parameterizes `POSTGRES_PASSWORD`/app-role credentials and
  requires `MDA_BOOTSTRAP_PASSWORD`.
- Migration `20260136000001` sets `tenant_id NOT NULL` on the role-keyed
  `sec.*` tables (backfilled by `20260114`; a NULL would fail closed by
  silently disappearing — now unrepresentable).
- Security suite no longer *silently passes* without `DATABASE_URL` (panics
  instead), asserts on response `total` (was `unwrap_or(0)` = vacuous), and
  the draft-PUT check says what it means (not-5xx, not success-or-client-error).

## Fourth pass (2026-08-16): correctness/consistency sweep — SSE scoping, SMTP egress, modeling gate

A fresh correctness/completeness/consistency/unambiguity review over docs +
crates (all suites green before and after). Three findings, fixed with
regressions:

### SSE `tenant:` channel was not caller-tenant-scoped (medium)

`GET /api/events?channel=tenant:<uuid>:broadcast` matched events for **any**
tenant id the client named — an authenticated user of tenant A knowing (or
guessing) tenant B's UUID received B's system events. Cross-tenant isolation
must not rest on the secrecy of tenant ids. The filter now requires
`tid == caller.tenant_id`; the `user:*:notifications` channel was already
self+tenant-scoped and gained regression coverage in the same unit suite
(`mda-api/src/events.rs`, `ChannelFilter` tests).

### SMTP header/envelope CRLF injection from modeler metadata (medium)

The §5.18 email channel interpolates the **notification-type label** (the
subject) and the **stored template content-type** — both modeler-authored,
§5.16-untrusted — directly into the DATA headers, and the envelope addresses
into `MAIL FROM`/`RCPT TO`. A label containing CRLF could split/inject headers
(a `Bcc:` exfil) or smuggle SMTP commands. All four values are now scrubbed of
CR/LF/NUL at the transport boundary (`header_scrub`, `mda-api/src/mail.rs`);
the body was already dot-stuffed.

### `reference`-typed *field* was publishable but unwritable (low, modeling UX)

`reference` is in the §5.6 type registry, so a modeler could publish a **field**
of that type — which `coerce()` then rejects on every write (a dead field).
Per §5.1/§5.7 a reference is modeled as a *relationship* (hoisted FK column);
the publish gate now rejects a `reference`-typed field with a message pointing
at relationships (`mda-meta/src/draft.rs`).

## Fifth pass (2026-08-22): publish-gate + GraphQL hardening

### Retiring an entity that a surviving relationship targets was publishable (medium)

`diff()` validated relationship targets against **active ∪ draft** entity ids,
so a draft that removed entity `B` while another entity's `→ B` relationship
survived passed validation — `B`'s id is still in the active half of the union.
Post-publish `B.status = 'retired'`, and every consumer that resolves the
target by name breaks: the GraphQL schema registers object types from *active*
entities only, so the next `build_schema()` fails (`Type ... not found`) and
**every GraphQL request for the tenant 500s** until the model is repaired. The
publish gate now (pass 3 in `diff()`, `mda-meta/src/draft.rs`) requires every
surviving relationship to target an entity also present in the draft, with a
message telling the modeler to remove the relationship first. Regression test:
`rejects_relationship_targeting_retired_entity`. Defense in depth: the GraphQL
reference resolver no longer `unwrap()`s the target lookup — it resolves null
(`crates/mda-api/src/graphql.rs`) instead of panicking on out-of-gate model
state.

## Sixth pass (2026-08-22): review — non-superuser migration role + webhook receiver

### `mda.lookup_webhook` returned nothing on non-superuser deployments (high)

The inbound receiver resolves `(tenant_id, secret_ref)` via the SECURITY DEFINER
function `mda.lookup_webhook` (`20260122000001`), whose comment assumed "the
migration role … bypasses RLS". That holds only when the migration role is a
SUPERUSER: migration `20260123000001` put `int.webhook` under ENABLE **and
FORCE** RLS, and under FORCE the table OWNER is subject to the policies too.
The lookup runs with no `app.tenant_id` GUC, so on any deployment whose
migrations run as a non-superuser owner — every managed Postgres (RDS, Cloud
SQL; they never grant superuser) — it saw zero rows and **every inbound webhook
was rejected with 404** while tests (superuser owner) stayed green. Fixed by
`20260137000001`: FORCE is dropped on `int.webhook` only. Tenant isolation is
unaffected — RLS stays ENABLED and every non-owner role (`mda_app` included)
remains fully subject to `tenant_isolation`; only the owner context, i.e.
exactly that one SECURITY DEFINER lookup, crosses tenants. Audited the whole
codebase for the same class: `lookup_webhook` was the only SECURITY DEFINER
function, and every other `int.*` reader sets the tenant GUC first
(`flow_for_webhook`, `flow_by_id`, the outbound relay's per-event transaction).
Regression test: `webhook_to_inbound_flow_materializes_via_drain`, which now
also passes under a non-CREATEROLE, non-superuser migration role.

### Unconditional `GRANT … TO mda_app` broke restricted fresh installs (medium)

Migration `20260111000001` deliberately treats `mda_app` as optional — skipped
where the migrating role lacks CREATEROLE — and every later migration guards
its grants with `IF EXISTS` … except `20260122000001`'s bare `GRANT EXECUTE ON
FUNCTION mda.lookup_webhook TO mda_app`, which hard-failed the entire chain
wherever the role legitimately does not exist. Editing an applied migration is
not possible (sqlx checksum validation fails startup on every healthy existing
database), so `20260137000001` re-issues the grant guarded + idempotently.
**Operational consequence:** a *fresh* install whose migration role cannot
create roles must have `mda_app` pre-created before first boot — release
deployments require the role anyway (`MDA_APP_DATABASE_URL`), so this only
bites debug-mode boots against managed Postgres, and it fails loudly with
`role "mda_app" does not exist` rather than silently.

## Verifying a deployment yourself

```bash
# release server as the production role, against your dev DB:
MDA_JWT_SECRET="$(openssl rand -hex 32)" \
MDA_APP_DATABASE_URL="postgres://mda_app:mda@127.0.0.1:5433/mda?sslmode=disable" \
DATABASE_URL="postgres://mda:mda@127.0.0.1:5433/mda?sslmode=disable" \
MDA_PORT=8080 cargo run --release --bin mda-server
```

Then: `GET /health`, login, publish a model, CRUD a record, create a webhook
subscription and watch `/api/webhooks/:id/deliveries` after a record write.

## One-time note for databases that lived through the pre-relocation era

Databases migrated between `20260110000001` and `20260125000001` **as role
`mda`** may carry tables in the `mda` schema. The relocation migrations
(25/32) handle the known sets. If boot fails with `relation "…" already
exists`, inspect `pg_tables WHERE schemaname = 'mda'` before doing anything
drastic — those rows are usually the live copies of platform tables and must
be moved (`ALTER TABLE … SET SCHEMA public`), not dropped. The local dev
database from this era was repaired in place; its pre-relocation duplicates
are preserved in the `mda_legacy_stale` schema and can be dropped once
verified.
