# Phase 10 — Bulk data & attachments (status & handoff)

**Status: complete & verified.** Implements PLAN §5.13 (bulk import/export) and
§5.14 (attachments & blob storage).

## Bulk import/export (§5.13)

- `POST /api/impex/:entity/import` — a JSON array of records; each row runs the
  **full runtime create pipeline** (RBAC + FLS + rules + calculated + audit), so
  an imported row is indistinguishable from one typed by hand. Best-effort;
  returns `{ imported, errors[{row,error}] }`.
- `GET /api/impex/:entity/export` — the filtered list as CSV (field-read security
  respected; reuses the report CSV renderer).

## Attachments (§5.14)

- `sys_blob` (metadata only — bytes live in a `BlobStore`) + `sys_blob_ref`
  (back-references for cleanup, §4.7).
- `BlobStore` trait + `LocalBlobStore` (dir via `MDA_BLOB_DIR`); S3 impl is a
  follow-up.
- `attachment` field type (stores a blob id in `attributes`).
- `POST /api/attachments` (raw body + `x-filename`/content-type) → `{id,…}`;
  `GET /api/attachments/:id` (owner/superuser; record/field attachment AuthZ +
  presigned URLs + virus-scan + dedup + orphan cleanup are follow-ups).

## Verification

`--test data`: bulk import (2/3 imported, 1 missing required) + export 200;
attachments upload→download + storing a blob id in an `attachment` field.

## Phase-10 decisions / deferrals

- Bulk import is **sync + best-effort** (JSON array). Deferred: CSV parsing,
  field-mapping UI, **dry-run** validation report, all-or-nothing mode, and the
  `sys_impex_job` async job for very large files (§5.13).
- Attachments: owner-based access for now; S3 store, presigned upload/download
  URLs, virus-scan hook, checksum dedup, thumbnails, and orphan cleanup are
  follow-ups.
