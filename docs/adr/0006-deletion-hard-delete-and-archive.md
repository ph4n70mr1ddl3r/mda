# ADR-0006: Deletion — hard-delete + archive (not soft-delete)

- **Status:** Accepted
- **Date:** 2025-07-16
- **Resolves:** REVIEW.md U3; makes §5.7 and §5.1 consistent
- **Detail:** PLAN.md §5.7, §5.1

## Context
Soft-delete (`deleted_at`) defeats native `ON DELETE CASCADE` (the row isn't actually deleted, so the FK action never fires), forcing RI to be re-implemented in the app layer — undercutting ADR-0001's native-FK benefit. It also complicates unique constraints (needs partial indexes `WHERE deleted_at IS NULL`).

## Decision
Deletion is **hard-delete + archive**: the row moves to `biz_archive.<table>` (carrying `archived_at` / `archived_by`); native cascade fires naturally; the archive provides undo/recoverability. Core columns carry **no `deleted_at`**. Soft-delete is rejected.

## Consequences
- **(+)** Native RI preserved end to end (ADR-0001); simpler unique constraints — a deleted value can be re-created immediately.
- **(+)** Recoverability retained via the archive table.
- **(−)** Restore from archive is an explicit operation (re-insert + re-run FK/cascade checks), not a flag flip.
