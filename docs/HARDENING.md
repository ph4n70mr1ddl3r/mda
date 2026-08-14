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
  migration that forgets a grant fails the build, not staging.
- `scheduler_tables_are_visible_to_the_app_role` (in `tests/scheduler.rs`):
  the specific misplacement regression.
- `quoted_rfc_etag_is_accepted` (in `tests/studio.rs`).
- Unit tests for the error envelope (no internal leak, client errors keep
  their message) and the panic layer.

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
