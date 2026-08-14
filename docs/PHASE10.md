# Phase 10 — Bulk data & attachments (status & handoff)

**Status: complete & verified.** Implements PLAN §5.13 (bulk import/export) and
§5.14 (attachments & blob storage).

## Bulk import/export (§5.13)

The synchronous impex contract — an import is *batched, mapped writes* reusing
the runtime write pipeline, so an imported row is indistinguishable from one
typed by hand (no second set of rules to drift).

- `POST /api/impex/:entity/import` — accepts **CSV** (`text/csv`) **or JSON**
  (array of objects). Each row runs the full create/update write service
  (RBAC + FLS + rules + calculated + audit + events).
  - `mode` = `create` (default) | `update` | `upsert`; `update`/`upsert` take a
    `key` field (a known field or `id`) and match under the caller's **write**
    scope (a user can only import-update a record they may write).
  - `dry_run=true` validates + resolves targets but writes nothing; returns
    `would_create` / `would_update` / per-row `errors`.
  - `on_error` = `continue` (default, best-effort per row) | `abort`
    (validate-then-commit ⇒ any error writes nothing — all-or-nothing).
  - Source columns auto-map by name to entity fields; unknown columns are a
    422 mapping error (up front, not per row). System columns (`id`, `owner_id`,
    …) are stripped before the write. A blank CSV cell = "not provided", so a
    required field left blank fails required (matching JSON semantics).
  - Returns `{ mode, format, dry_run, on_error, created, updated, imported,
    would_create, would_update, errors[{row,error}] }`.
- `GET /api/impex/:entity/export` — the filtered list as CSV (field-read security
  respected; reuses the report CSV renderer, which now RFC-4180 quotes). A
  round-trip works: export → edit → `import?mode=upsert&key=id`.

## Attachments (§5.14)

- `sys_blob` (metadata only — bytes live in a `BlobStore`; incl. a sha256
  `checksum` for integrity + dedup) + `sys_blob_ref` (back-references for
  cleanup, §4.7 — the record→blob lifecycle hook is a follow-up).
- `BlobStore` trait (put/get/**delete**) + `LocalBlobStore` (dir via
  `MDA_BLOB_DIR`); S3 impl is a follow-up.
- `attachment` field type (stores a blob id in `attributes`).
- `POST /api/attachments` (raw body + `x-filename`/content-type) →
  `{id, filename, mime, size, checksum}`. Computes sha256 and **dedups by
  `(tenant, checksum)`**: two uploads of the same bytes share one stored blob
  (a fresh metadata row per upload, so ownership/metadata stay independent).
- `GET /api/attachments/:id` (owner/superuser; record/field attachment AuthZ +
  presigned URLs + virus-scan + thumbnails are follow-ups).
- `DELETE /api/attachments/:id` (owner/superuser) — removes the metadata row
  and reclaims the bytes **only when it was the last reference** (refcount on
  `storage_key`), so a dedup-shared blob is never orphaned by deleting one ref.
  Idempotent byte cleanup.

## Verification

`--test data`: bulk import (JSON create, best-effort), plus the new §5.13
contract — CSV import (quoting round-trips), `dry_run` (writes nothing),
`upsert`/`update` by key (create + update + missing-key row error),
`on_error=abort` (all-or-nothing), unmapped-column 422, and a full export →
import round-trip. `mda-reports --lib`: RFC-4180 `from_csv`/`to_csv` round-trip.

## Phase-10 decisions / deferrals

- The §5.13 **synchronous** surface is complete: CSV+JSON, create/update/upsert,
  dry-run, on_error abort/continue, key-field matching, column mapping. Still
  deferred (lower priority / large-file scale): the async `sys_impex_job` worker
  for very large files (streaming, resumable per-row results, downloadable error
  report), XLSX, and the Studio mapping UI. Reference-field **lookup** (resolve a
  Customer by `name` rather than id) is a natural follow-up on the same boundary.
- Attachments now ship sha256 checksum + dedup + refcount-aware delete. Still
  deferred: the S3 store, presigned upload/download URLs, virus-scan hook,
  thumbnails, and the record→blob `sys_blob_ref` lifecycle hook (clear an
  attachment field on record hard-delete → orphan cleanup).
