# ADR-0002: Metadata lifecycle — draft/publish + data migration

- **Status:** Accepted
- **Date:** 2025-07-16
- **Resolves:** REVIEW.md C2, C3
- **Detail:** PLAN.md §5.8, §4.8

## Context
Live metadata describes data that already exists and that other metadata depends on. Freely mutating it is unsafe: changes may need DDL + data transformation, must be validated and previewable, and must be rollbackable.

## Decision
All metadata edits go through a **draft → validate → publish → activate** lifecycle. Drafts (JSONB) are editable; the active model lives in the normalized `md_*` tables; snapshots archive history. Publish computes `diff(active, draft)`, classifies ops (additive / transforming / two-phase destructive), runs validated DDL + data migrations atomically under a per-tenant advisory lock, then promotes. There is no "edit the active model directly."

## Consequences
- **(+)** One validated path to change the model; preview and rollback supported.
- **(+)** The same diff drives both the `biz` schema/data and the `md_*` tables.
- **(−)** Destructive changes are two-phase (retire → purge after grace); rollback cannot restore already-purged data.
