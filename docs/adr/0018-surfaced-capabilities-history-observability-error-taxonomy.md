# ADR-0018: Surfaced platform capabilities — record history/as-of, observability console, error taxonomy

- **Status:** Accepted
- **Date:** 2025-07-18
- **Detail:** PLAN.md §14 (tracked, not-yet-designed platform gaps)

## Context

PLAN §14 lists real but lower-priority platform gaps that were deliberately left
unsurfaced during the phase build-out so they would not be "rediscovered
mid-build". Three of them are pure read-paths over data the platform **already
writes**, so closing them is high-value and low-risk:

1. **Record / field history as a surfaced capability** — `sys_audit_log` stores a
   before/after JSONB snapshot for every write (compliance, §4.7), but "timeline
   of this record" and "as-of" queries were not exposed as an API.
2. **Modeler / tenant observability console** (+ **scheduled-job failure state**)
   — `sys_event_log`, `sys_outbox`, `md_migration_log`, and `sys_audit_log` are
   the raw material for a tenant-facing view of job/rule/workflow/publish/delivery
   run history; only operator `tracing`/OpenTelemetry exposed any of it.
3. **Error code taxonomy + localized error messages** — the API returned 4xx/5xx
   with ad-hoc `error`/`message` blobs and no stable, machine-readable code an
   SDK could branch on or a translator could key on.

## Decision

### 1. Record history & as-of (read straight from `sys_audit_log`)

- `GET /api/data/:entity/:id/history` — a newest-first timeline of changes with
  **per-field diffs** (`from`/`to`), each side projected through field-level
  security. Internal versioning columns are never reported as changes.
- `GET /api/data/:entity/:id/as-of?version=N` (or `?at=<RFC3339>`) — reconstruct
  the record state at a point/version directly from the audit `after` snapshots.
  Returns `mda.not_found` if it did not exist yet or had been deleted by then.
- **Authorisation mirrors a live read**: object-level `read`; record-level — the
  caller must be able to read the *live* record, or be a superuser (a deleted
  record's history is forensics/admin-only in v1, consistent with restore being
  admin-only); field-level projection on every snapshot. No separate history
  store — the same source the real-time channel and compliance trail use, so the
  three never disagree.

### 2. Tenant observability console (superuser-gated)

Four read-only, tenant-scoped endpoints:
`/api/observability/{events,outbox,migrations,audit}`. **Superuser-only** in v1:
the console aggregates every entity's activity and returns audit `before`/`after`
verbatim (a superuser already bypasses field-level security). A follow-up can
introduce a scoped `observability.read` capability with field-level projection
for non-admin tenant modelers. `sys_*` tables are app-layer-isolated (no RLS);
`md_migration_log` is read under the tenant GUC like the Studio handlers.

### 3. Error code taxonomy (the SDK / i18n contract)

Every `Error` now exposes `code() -> &'static str` — a stable, namespaced
identifier (`mda.invalid`, `mda.not_found`, `mda.conflict`, `mda.forbidden`,
`mda.rate_limited`, `mda.config_error`, `mda.internal_error`). The API error
envelope carries `code` (canonical), `status` (HTTP), the legacy `error` bucket,
and `message` (English dev string). **Codes never change for a variant** (new
variants get new codes); they are the i18n key and the SDK switch target.

### Bonus: the publish execution log is now populated

`md_migration_log` existed as a schema placeholder for ADR-0011's staged
execution but nothing wrote to it. `apply_additive_publish` now records one row
per executed op category (`create_table`, `add_column`, `add_relationship`,
`retire_entity`, `retire_field`) with `status='done'` and a real `rows_affected`,
inside the publish transaction — the honest, additive seed that ADR-0011's
resume/revert checkpoints extend later, and what the migrations console surfaces.

## Consequences

- **+** Three §14 gaps closed with zero new write paths or stores — they read
  material the platform already produces, behind the existing AuthZ model.
- **+** SDK clients get a stable error contract; translators get message keys.
- **+** The publish log is no longer a dead table; observability is meaningful.
- **−** Observability is admin-only in v1 (non-admin tenant modelers cannot yet
  see it). Explicit; lifted by a future scoped capability.
- **−** `as-of` is reconstructed from audit snapshots, so it is bounded by audit
  retention (§5.15). Beyond the retention window it degrades to `not_found`,
  which is the correct, safe behaviour.
- The history/observability reads are covered by DB-backed E2E tests
  (`tests/data.rs`, `tests/observability.rs`): timeline + diffs, FLS projection,
  record-level hiding, as-of reconstruction, deleted-record admin-only, admin
  gating, tenant scoping, and the stable error code.
