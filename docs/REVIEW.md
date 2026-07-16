# Review of PLAN.md

A critical, self-imposed architectural review. Verdict up front, then strengths, then the gaps that will actually hurt.

> **Status (v0.2):** All critical gaps (C1–C6) and underspecified items (U1–U4, U6–U8) are now resolved in `PLAN.md`; the timeline re-estimate and MVP milestone are addressed in §9. Only **U5 (data i18n)** and **U9 (HA/replication)** remain — deliberately deferred to later phases. This review is retained as the reasoning trail. See `adr/` for the recorded decisions.

---

## Verdict

The plan is a **sound skeleton** — good layering, sensible tech choices (SQLx over SeaORM is the right call for a dynamic-data system), honest flagging of hard decisions, and a phased roadmap with demoable deliverables. It would pass as a credible "v0 architecture proposal."

But as a *build plan* for a metadata-driven enterprise platform, it has **serious underspecification in exactly the places that determine success or failure**: the dynamic-data semantics, the metadata lifecycle, referential integrity, concurrency, and real-time UI. Several "recommendations" quietly hand-wave the hardest 20% of the work. Left as-is, Phase 2 will surface contradictions that should have been resolved in Phase 0.

Below: the gaps ranked by how much pain they'll cause.

---

## Critical Gaps (fix before Phase 0)

### C1. Referential integrity for the JSONB-per-entity model is unaddressed
§5.1 recommends JSONB documents per entity, and the field registry (Phase 2) includes `reference`. But:
- You **cannot** use a real PostgreSQL FK on a value stored inside JSONB `attributes`.
- So how is "Invoice.customer_id → Customer.id" enforced? Cascade rules? Orphan cleanup? Blocking delete?
- Reporting joins across entities (`invoices JOIN customers ON …`) work but are uglier and slower than the plan implies.

This is the single biggest hole. Enterprise systems live or die by RI. The plan must specify a strategy: either (a) hoist reference values into real typed columns on the `biz.<table>` (a mini-hybrid from day one), (b) maintain RI via triggers + a metadata-driven constraint layer, or (c) accept eventual-consistency cleanup jobs. Each has real tradeoffs that need an explicit decision and an ADR.

### C2. There is no draft → publish → activate lifecycle for metadata
The plan implies "edit md_entity → it takes effect," but Phase 6 mentions `/api/studio/publish`. So which is it? This is central:
- Can a user edit a *published, in-use* entity? (Data already exists in `biz.<table>`.)
- How do you validate a change won't break existing rows (rename field, change type, make required)?
- How does the Studio UI *preview/test* a form against draft metadata before publishing?

Real platforms all have a **draft model vs. published model** (often versioned). The plan treats metadata as live-editable, then mentions publish. This contradiction will cost weeks if not resolved up front. Recommend: draft model (editable) + immutable published snapshots; runtime only ever reads published; publish runs a validation + migration gate.

### C3. Data migration on schema change is one bullet — it needs to be a sub-system
§5.5: "destructive changes require a migration job." Okay — but **what** runs, when, and how? Concretely, when a modeler:
- renames a field,
- changes a type (string → number),
- makes an optional field required,
- deletes a field,
- changes a reference target entity,
…existing JSONB rows must be transformed. This is a **migration engine over live data**, driven by the diff between two published model versions. It needs its own design (dry-run, batching for large tables, rollback, validation report). It is not a footnote. This is where most homegrown low-code platforms quietly die.

### C4. Concurrency & transactional semantics are unspecified
- **Record locking**: `sys_lock` is listed but never designed. Lost-update prevention (optimistic via `version`/etag, or pessimistic) is mandatory for an enterprise CRUD system and is absent.
- **Transactional boundaries for workflows/rules**: a transition that updates a record, writes a task, enqueues a notification, and fires a webhook — is that one DB transaction? If the webhook is async (it should be), how is the "enqueue" durable? (Transactional outbox pattern is the answer; it's not mentioned.)
- **Rule execution**: synchronous in the caller's transaction, or out-of-band? This single choice determines whether rules are "trustworthy" or "eventual." Not decided.

### C5. No real-time channel for the runtime UI
A metadata-driven UI where two users edit the same record, or a workflow silently moves a record, **must** push updates or the UX collapses into stale data. The plan mentions LISTEN/NOTIFY only for cache invalidation. Decide now: WebSocket/SSE fan-out driven by the event log. Polling a dynamic API is a non-starter at scale and a UX disaster.

### C6. Authorization is too coarse for enterprise
`sec_permission` = "verb on entity/view/report." Missing:
- **Field-level security** (role A cannot read `salary`, cannot write `discount > 10%`). Universal requirement.
- **Action/transition-level security** (who may invoke the "Approve" transition?).
- **Record-sharing model** (Salesforce-style sharing rules, or at least team/owner + explicit shares) beyond simple owner-based ABAC.

ABAC via the expression engine is the right primitive, but the permission *model* needs field-level and action-level granularity designed in, not bolted on.

---

## Underspecified / Missing Entirely

### U1. Bulk data import/export at the record level
Nowhere in scope. Enterprise day-1 requirement: CSV/Excel upload with field mapping, validation report, dry-run, partial-failure handling. The plan covers *metadata* export/import but not *data*. Add a phase or fold into Phase 2/6.

### U2. Attachments / blob storage
"Blob store" appears once in the architecture diagram. But attachments-on-records (a Customer has signed PDFs) need: a field type, storage backend abstraction (S3/local), signed URLs, virus scan hook, retention. Not designed. Add an `attachment` field type and a storage trait in Phase 2.

### U3. Soft-delete vs. uniqueness interaction
Soft delete (`deleted_at`) + unique email means you can't re-create a record after deletion without conflict. Requires **partial unique indexes** `WHERE deleted_at IS NULL` — generated as part of the DDL automation. Not mentioned; will bite immediately.

### U4. Audit log growth
`sys_audit_log` storing before/after JSONB on every write grows unbounded. Needs partitioning by time, retention policy, archival to cold storage, and probably sampling for high-volume entities. Absent.

### U5. i18n of *data*, not just metadata
`sys_translation` is implied for UI strings. Enterprise systems also need translatable enum/reference data, sometimes record-level (multi-language product names). Scope this explicitly or defer with a clear "v2" marker.

### U6. Expression engine safety
A custom AST evaluator avoids code injection, but:
- **Resource limits**: max depth, max nodes, max function-call budget per evaluation. A pathological expression or a self-triggering rule cascade can DoS the system.
- **Rule cycle detection**: if rule A's action triggers rule B's event, which triggers A… Need a depth cap and cycle guard.
- **Determinism in async**: rule firing order matters; specify a deterministic ordering (priority then id).

### U7. Idempotency & DLQ for async workers
Webhooks/ETL/scheduled reports need retry with exponential backoff + jitter, idempotency keys, and a dead-letter queue with replay. The job-queue choice (`apalis` vs `tokio-cron-scheduler`) is left open and these have **very different** reliability semantics. Just pick `apalis` (persistent, retries) and design the outbox.

### U8. Self-describing meta-metadata
The platform's own model (md_entity, md_field…) is static Rust + SQL. Pragmatic, but it means the Studio UI **cannot** treat its own definitions as first-class entities (Salesforce does, via `EntityDefinition`). Decide explicitly: is the meta-model self-hosted (elegant, harder) or fixed (pragmatic, the recommendation)? The plan is silent; an ADR should record the choice and its consequence (no "edit the editor").

### U9. High availability / replication
PostgreSQL HA, connection-pool sizing, read replicas for reports, logical replication for warm standby — absent from the ops picture. Fine to defer to Phase 10, but call it out as deferred, not omitted.

---

## Specific Technical Corrections

| § | Issue | Fix |
|---|---|---|
| 5.1 | "JSONB document" + "auto-provision `biz.<table>` on publish" is effectively **table-per-entity** (strategy B), not pure C. The C-vs-B distinction collapses once you generate real tables. | Re-frame: "one generated table per entity, with a stable core schema + JSONB `attributes`." Hoist reference FKs and unique-constrained fields into real columns at publish. That's the honest model. |
| 5.2 | "Rhpr" is a typo (meant `rhai`? `rune`?). | Pick `rhai` (sandboxed, embeddable) or `rune`; remove the stray. |
| 5.3 | LISTEN/NOTIFY is good but lossy across reconnects and doesn't span replicas. | Use it as a fast path **plus** a version-stamp poll fallback, or use Redis pub/sub as primary. |
| 5.6 | "dynamic loading via `wasmtime` later" for field types — worth noting this also enables **tenant/customer-specific logic** safely, a major differentiator. | Elevate to a first-class extensibility story. |
| 7 | GraphQL dismissed as "optional." For a dynamic data API with relationships, GraphQL is actually a strong fit (clients traverse references). | At least prototype it; consider it primary for the runtime data API. |
| 8 | Leptos-for-Studio hedged. | The drag-and-drop Studio designers are the **single highest-risk, highest-effort** component. Make a hard call or explicitly de-risk with a spike in Phase 0. Don't leave it ambiguous to Phase 8. |
| 10 | Testing listed generically. | The **expression evaluator and the dynamic query builder are the two highest-blast-radius components** — a bug affects every entity. Mandate fuzzing + property tests + a corpus of golden expressions/queries for these specifically. |
| 11 | "Security holes in dynamic API" row is generic. | Add: a dedicated **threat model** for "untrusted metadata" — because clients/studio users are authoring logic (rules, expressions, field defs) that the engine executes. This is a unique attack surface vs. normal apps. |
| 12 | Missing ADR list. | Pre-create ADR stubs for C1–C6 so they're tracked. |

---

## Timeline Reality Check

Even with the "solo dev should double" caveat, several phases are dangerously compressed:

- **Phase 6 (5 weeks): Forms + Views + a from-scratch Leptos dynamic UI renderer.** A metadata-driven form renderer handling all field types, client-side validation, conditional visibility, dependent/reference lookups, inline grids, and a dashboard shell is *easily* 5 weeks on its own — before views. **Realistic: 8–10 weeks.**
- **Phase 8 (6 weeks): Full Studio UI** with drag-and-drop designers for entities, forms, views, reports, workflows, rules, and security. This is the largest single chunk in the entire plan and it's compressed. **Realistic: 12–16 weeks**, and only parallelizable with a real team.
- **No explicit MVP cut.** The plan treats all ten phases as "v1.0." A credible MVP is: **define entity → CRUD via API → basic form/list UI → auth.** Everything else (workflow, reporting, integration, Studio) is post-MVP. Recommend an explicit "Phase 4-ish MVP" milestone to get something in users' hands and de-risk.

---

## What's Good (keep these)

- Cargo workspace with clean crate boundaries — correct instinct.
- SQLx over SeaORM — right for dynamic SQL.
- LISTEN/NOTIFY + moka cache — sound invalidation approach (with the version-stamp caveat).
- Phased, demoable roadmap — good discipline.
- Honest §5 decision-flagging — better than most plans.
- "Every deliverable backed by an E2E test that defines metadata and exercises the runtime" — the golden rule; excellent.

---

## Recommended Amendments to PLAN.md

1. **Add §5.7 Referential Integrity strategy** (hoist FKs/unique fields to real columns; reference constraints via metadata layer).
2. **Add §5.8 Draft/Publish model + data migration engine** (the biggest missing sub-system).
3. **Expand §5.6 extensibility** to cover field-level/action-level security and the threat model of untrusted metadata.
4. **Add a real-time push channel** (WebSocket/SSE over the event log) to the architecture diagram and a sub-phase in Phase 6.
5. **Add bulk data import/export + attachments** as scoped items (Phase 2 / Phase 6).
6. **Pick the job queue** (`apalis`) and specify the transactional outbox + DLQ pattern.
7. **Insert an explicit MVP milestone** around the end of Phase 4 (auth + dynamic CRUD + basic form UI).
8. **Re-estimate Phases 6 and 8** honestly; add a Phase 0 spike to de-risk the Leptos-vs-React Studio decision.
9. **Pre-create ADR stubs** for C1–C6 and record them as the Week-1 deliverable.

---

## Bottom Line

The plan is a good **architecture overview** but not yet a **build plan**. The good ideas are at the right altitude; the hard ideas are under-specified. Resolve C1–C6 (referential integrity, draft/publish + migrations, concurrency, real-time, fine-grained auth) before writing application code — they are architectural and expensive to retrofit. With those amendments plus an honest MVP cut and re-estimated UI phases, this becomes a plan you can actually execute.
