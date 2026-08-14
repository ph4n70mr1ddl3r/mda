# ADR-0021: Mass actions — bulk update/delete by filter

- **Status:** Accepted
- **Date:** 2026-01-28
- **Resolves:** PLAN §9 "Mass actions" (was a tracked, unscheduled deferral)
- **Detail:** PLAN §9 / §5.13; implementation `crates/mda-api/src/data.rs`
  (`mass_update_record`, `mass_delete_record`) + `crates/mda-data/src/crud.rs`
  (`mass_target_ids`)

## Context

§9 tracked "mass actions — bulk update / delete / assign / transfer *by filter*
(distinct from file import/export, §5.13); interacts with sharing recompute
(ADR-0013) and cascade (ADR-0006), so it needs its own design rather than a
retrosfit" as an open deferral. The §5.13 file import is row-by-row from an
uploaded file; a mass action is a single declarative operation over a live
filter — semantically "apply this patch to / remove every record that matches,
that I may write". Without it, operators fall back to external scripts that
bypass the write pipeline (and therefore AuthZ, rules, audit, and the event
log), which is exactly the footgun a metadata-driven platform must close.

## Decision

Ship **bulk update / delete by filter** as first-class runtime routes that reuse
the single-record write pipeline *per affected record*:

- `POST /api/data/:entity/mass-update` — `{ filter, set, dry_run?, limit? }` →
  `{ affected, ids, errors[], dry_run }`.
- `POST /api/data/:entity/mass-delete` — `{ filter, dry_run?, limit? }` →
  `{ affected, ids, errors[] }`.

Target resolution (`mda_data::mass_target_ids`) injects the **write** predicate
(not the read predicate `list` uses), so a mass action can never reach a record
the caller may not write — the same scope a single PATCH/DELETE enforces. The
result is ordered by id and capped (`MAX_MASS_BATCH = 5000`, overridable down)
so a broad filter is bounded and resumable.

For each target id, the action calls the **same shared write services** REST
and GraphQL use (`update_record_service` / `delete_record_service`):
RBAC verb check, FLS write-check (validated on the patch *upfront*, before any
record is resolved, so a forbidden patch is rejected immediately), rules +
calculated fields, OCC (the record's current version is read in-loop and
passed as `If-Match`, so a row changed between resolution and the write is
skipped with a `mda.conflict` per-row error rather than clobbered), audit
before/after, and a `record.updated`/`record.deleted` event per record. A mass
update is therefore indistinguishable from N hand-typed PATCHes and inherits
record-level security, cascade archive (ADR-0006), and sharing recompute
(ADR-0013) by construction.

`dry_run: true` resolves + returns the candidate id set without mutating.

## Consequences

- No second write path: mass actions share the service layer, so security,
  rules, audit, and event-log behaviour cannot drift from single-record writes.
- OCC is preserved per record — a concurrent editor is never silently
  overwritten; their stale write later conflicts on the bumped version.
- The per-record loop is O(n) round-trips, bounded by the cap. For very large
  fan-out a future async `apalis` job variant (ADR-0007) can drain the same
  target set; the API contract (`affected`/`ids`/`errors`) is unchanged.
- FLS on the patch is checked once up front (not per record) — a mass update
  writing a non-writable field is a 403 before any record is touched, matching
  single-record PATCH semantics.
