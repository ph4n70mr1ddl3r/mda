# Architecture Decision Records

ADRs record architecturally significant decisions for MDA — ones that are hard
to reverse and that constrain implementation. Format follows Michael Nygard's
template (**Status / Context / Decision / Consequences**).

The full reasoning for each decision lives in `PLAN.md`; each ADR summarizes the
decision and links back.

## Index

| # | Title | Status |
|---|---|---|
| [0001](0001-storage-and-referential-integrity.md) | Storage model & referential integrity (Pattern B) | Accepted |
| [0002](0002-metadata-lifecycle-draft-publish-migration.md) | Metadata lifecycle: draft/publish + data migration | Accepted |
| [0003](0003-concurrency-and-transactional-outbox.md) | Concurrency & transactional outbox | Accepted |
| [0004](0004-real-time-channel.md) | Real-time channel (SSE over the event log) | Accepted |
| [0005](0005-multi-grained-authorization.md) | Multi-grained authorization | Accepted |
| [0006](0006-deletion-hard-delete-and-archive.md) | Deletion: hard-delete + archive (not soft-delete) | Accepted |
| [0007](0007-job-queue-apalis.md) | Job queue: apalis (Postgres-backed) | Accepted |
| [0008](0008-meta-model-fixed.md) | Meta-model: fixed, not self-hosting | Accepted |
| [0009](0009-frontend-strategy-leptos.md) | Frontend strategy: Leptos (WASM) | Accepted |
| [0010](0010-graphql-first-class-runtime-api.md) | GraphQL as a first-class runtime data API | Accepted |
| [0011](0011-publish-execution-staged-migration-and-atomic-cutover.md) | Publish execution: staged migration + atomic cutover | Accepted |
| [0012](0012-value-constraint-composition-intersection.md) | Value-constraint composition across multiple roles (intersection) | Accepted |
| [0013](0013-sharing-materialization-revoke-safe-epoch-invalidation.md) | Sharing materialization: revoke-safe via epoch invalidation | Accepted |
| [0014](0014-reporting-query-model-structured-metadata.md) | Reporting query model: structured metadata, runner-context AuthZ by construction | Accepted |
| [0015](0015-deletion-and-restoration-lifecycle.md) | Deletion & restoration lifecycle: trigger-driven archive, batch restore, cold-storage purge | Accepted |
| [0016](0016-chained-workflow-transitions-sync-default-async-with-failure-handling.md) | Chained workflow transitions: sync-by-default (atomic) vs async (with required failure handling) | Accepted |
| [0017](0017-rollup-summaries-incremental-sync-default-async-opt-out.md) | Rollup summaries: incremental sync by default, async opt-out for hot parents | Accepted |
| [0018](0018-surfaced-capabilities-history-observability-error-taxonomy.md) | Surfaced platform capabilities: record history/as-of, observability console, error taxonomy | Accepted |
| [0019](0019-scheduled-job-management.md) | Scheduled-job management: cron-driven scheduler | Accepted |

## Status legend
`Proposed` → `Accepted` → `Deprecated` / `Superseded by ADR-NNNN`.
