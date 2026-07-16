# ADR-0015: Deletion & restoration lifecycle — trigger-driven archive, batch restore, cold-storage purge

- **Status:** Accepted
- **Date:** 2025-07-16
- **Refines:** ADR-0006 (deletion: hard-delete + archive)
- **Detail:** PLAN.md §5.7, §5.8, §5.15

## Context

ADR-0006 chose hard-delete + archive over soft-delete (to keep native cascade
working), but left two gaps:

1. **Restore semantics for cascades + version (B10).** §5.7 says restore
   "re-inserts from `biz_archive` (re-running FK/cascade checks)." But hard-delete
   fires native `ON DELETE CASCADE` — deleting a parent also removes its cascade
   children. Are those children archived and restored together? And what `version`
   does a restored row get (it must not collide with OCC history, §5.9)? The
   archive mechanism itself — how rows reach `biz_archive` — was unspecified.
2. **Destructive metadata purge vs recoverability (B9).** §5.8 Phase 2 purge
   "drops the column/table for real. Irreversible." ADR-0006 archives deleted
   *records*, but a purged *column or entity* (metadata-level destructive) has no
   archive step. For compliance-heavy tenants, destroying data on purge without an
   export may be a legal violation; yet the live schema is gone, so it cannot be
   restored to `biz` either. The purge→archive contract was undefined.

## Decision

### Two archive tiers — distinct purpose, distinct restorability

| Tier | Trigger | Location | Purpose | Restorable? |
|---|---|---|---|---|
| **Operational** `biz_archive.<table>` | record hard-delete (+ its cascades) | in-DB, per-entity mirror | undo accidental deletes; recoverability | **yes** — batch re-insert (schema intact) |
| **Cold-storage export** | metadata purge (drop column/table/entity, §5.8) | S3/Parquet, off-DB | compliance retention of retired-schema data | **no** — schema gone; compliance reads only |

### Operational archive is trigger-driven (captures the whole cascade tree)

Each `biz.<table>` has a `BEFORE DELETE` row trigger that copies the
about-to-be-deleted row into `biz_archive.<table>` (carrying `archived_at`,
`archived_by`, `archive_batch_id`). Because every table in a cascade has its own
trigger, native `ON DELETE CASCADE` archives **transitively**: deleting a
master-detail parent archives the parent plus all cascade-deleted children under
one `archive_batch_id`. `SET NULL` children are not deleted (their ref is
nulled) so are not archived; `RESTRICT` aborts (nothing archived, nothing
deleted). The archive set is exactly the rows physically removed by the delete
and its cascades.

### Restore is batch-scoped, dependency-ordered, version-bumped

- **Batch restore (default).** Restoring a deletion event = re-insert all rows
  sharing an `archive_batch_id`, in dependency order (parents before children), in
  one transaction. Re-insertion re-runs FK/cascade checks (ADR-0006), so a restore
  whose parent (outside the batch) is still gone, or that a now-`RESTRICT`
  relationship blocks, fails cleanly.
- **Version.** The restored row gets a **new `version` higher than the archived
  one** (archived + 1) and a fresh `updated_at`. Any client that somehow held the
  pre-deletion version receives a clean **409** on retry (OCC, §5.9) rather than a
  silent update. ULID ids never collide, so re-inserting the same id is safe (it
  was hard-deleted).
- **No auto-relink of `SET NULL` refs.** Restore brings back the physically-deleted
  rows only. A lookup child whose ref was `SET NULL` during the delete keeps its
  null/changed ref — restore does not re-point it. Predictable: restore means
  "undelete these rows," not "undo every side effect."

### Purge always exports to cold storage before the irreversible drop (B9)

Phase 2 purge (§5.8) remains irreversible to the **live schema** (the metadata
definition is retired), but the data is never silently destroyed:

- **Drop column:** before `ALTER TABLE … DROP COLUMN`, export the column's values
  (per row) to cold storage keyed by `(tenant, entity, field, purged_at)`.
- **Drop entity (table):** before `DROP TABLE`, export the entire `biz.<table>`
  **and** its `biz_archive.<table>` (the record-level deletes during the retire
  grace), then drop both.
- **Retention:** cold-storage purge exports follow audit retention (§5.15, 1–7 yr,
  configurable per tenant), then are deleted.
- **Restorability:** not a one-click restore — the field/table definition is gone
  from metadata. "Undoing" a purge = re-create the field via publish (§5.8) and
  bulk-import from cold storage (§5.13), an explicit, manual operation. Consistent
  with §5.8's "rollback cannot restore already-purged data."

### Operational archive retention

`biz_archive.<table>` is an undo store, not a compliance store — it carries a
per-tenant **undo-TTL** (default 30–90 days) after which rows are dropped (or, on
entity purge, exported to cold storage with the live data). This prevents
unbounded growth (§5.15) and keeps recent-undelete queries cheap.

## Consequences

- **(+)** Cascades are captured for free — the `BEFORE DELETE` trigger plus native
  cascade do the work; no app-layer cascade reimplementation (consistent with
  ADR-0001 / ADR-0006).
- **(+)** Restore is safe and predictable: batch-scoped (no orphaned children),
  dependency-ordered, OCC-safe (version bump), and explicitly does not relink
  `SET NULL` refs.
- **(+)** Purge is compliance-safe (cold-storage export before drop) without
  pretending a dropped column can be one-click restored — the irreversibility is
  honest and scoped to the live schema.
- **(+)** Two clean tiers with distinct retention and restorability; no single
  archive trying to serve both undo and compliance.
- **(−)** A `BEFORE DELETE` trigger per `biz` table plus an `archive_batch_id`
  column — one extra insert per delete (minor write-path overhead; deletes are far
  less frequent than reads/writes).
- **(−)** Restoring a deeply cascaded delete re-inserts many rows in dependency
  order in one transaction — bounded by cascade-tree size. Large cascade restores
  may need batching (rare; deletes cascading thousands of children are unusual).
