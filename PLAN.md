# MDA — Model-Driven Architecture Enterprise Platform

A declarative, data-driven, model-driven no-code enterprise system built entirely in Rust. Everything — entities, forms, screens, reports, workflows, rules, integrations — is stored as metadata in the database and interpreted by a runtime engine.

---

## 1. Vision & Goals

### What we're building
A platform where the "application" is data, not code. Business analysts define entities, forms, screens, reports, and workflows through a Studio UI; a runtime engine interprets that metadata to deliver a fully functional enterprise system.

### Core principles
1. **Metadata is king** — Every artifact (entity, field, form, report, workflow, rule) is a row in a metadata table. No code changes to ship new business logic.
2. **Single source of truth** — The database holds the model *and* the data. The model is queryable, versionable, and migratable.
3. **Late binding / interpretation** — The engine reads metadata at request time (with aggressive caching) rather than generating code.
4. **Extensible** — Pluggable field types, custom functions, scripting (via a sandboxed expression language), webhooks, and connectors.
5. **Multi-tenant ready** — Schema isolation strategy from day one (tenant_id or schema-per-tenant).
6. **Auditable** — Every record change, workflow transition, and security decision is logged.
7. **API-first** — Everything the UI can do, the API can do.

### Non-goals (for v1)
- Code generation / compile-step deployment (we interpret, not generate)
- Mobile-native apps (responsive web first)
- Real-time collaborative editing of metadata

---

## 2. High-Level Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                       CLIENTS                                 │
│  Studio UI (admin)    │  Runtime UI (end users)  │  External  │
│  (build models)       │  (use the system)        │  API/SDK   │
└───────────┬───────────┴────────────┬────────────┴──────┬─────┘
            │                        │                   │
┌───────────▼────────────────────────▼───────────────────▼─────┐
│                        API GATEWAY                            │
│   AuthN (JWT/OAuth) · AuthZ (RBAC+ABAC) · Rate limit · Audit  │
└───────────┬───────────────────────────────────────────────────┤
            │                                                   │
┌───────────▼─────────────────────────────────────────────────┐ │
│                    APPLICATION CORE                          │ │
│  ┌─────────────┐  ┌──────────────┐  ┌────────────────────┐   │ │
│  │  Metadata   │  │   Runtime    │  │  Service Layer     │   │ │
│  │   Engine    │◀▶│   Engine     │◀▶│  (orchestration)   │   │ │
│  │ (definitions)│  │ (interpret)  │  │                    │   │ │
│  └─────────────┘  └──────────────┘  └────────────────────┘   │ │
│  ┌───────────┐ ┌───────────┐ ┌──────────┐ ┌──────────────┐   │ │
│  │ Workflow  │ │ Reporting │ │  Rules   │ │ Integration  │   │ │
│  │  Engine   │ │  Engine   │ │ Engine   │ │   Layer      │   │ │
│  └───────────┘ └───────────┘ └──────────┘ └──────────────┘   │ │
└───────────┬───────────────────────────────────────────────────┘ │
            │                                                   │ │
┌───────────▼─────────────────────────────────────────────────┐ │
│                    PERSISTENCE                                │ │
│  PostgreSQL:  metadata schema  │  data schema (dynamic)      │ │
│               audit/event log  │  blob store                 │ │
└──────────────────────────────────────────────────────────────┘ │
                                                                  │
┌──────────────────────────────────────────────────────────────┐ │
│  ASYNC WORKERS:  workflow timers · scheduled reports · ETL  │◀┘
│                  webhooks · integrations · email            │
└──────────────────────────────────────────────────────────────┘
```

### The two-data-model insight (critical)
The database holds **two distinct models**:
1. **Metadata model** — statically-typed tables (`md_entity`, `md_field`, `md_form`, etc.) that *describe* the application. Fixed schema, known at compile time.
2. **Business data model** — the dynamic instances of those entities. Stored in a **single generic table** (EAV-ish) OR in **generated/dynamic tables** per entity.

> **Decision (recorded): Pattern B — real table per entity + native Postgres FK constraints, JSONB only for the non-relational remainder.** See §5.1, §5.7, and `docs/ri-strategies.md`.

---

## 3. Technology Stack

| Concern | Choice | Why |
|---|---|---|
| Language | Rust (stable) | Safety, performance, no GC pauses |
| Web framework | **Axum** | Tower middleware ecosystem, simple, async, great with Tokio |
| Async runtime | Tokio | De facto standard |
| Database | **PostgreSQL 16** | JSONB, row-level security, LISTEN/NOTIFY, mature |
| DB access | **SQLx** (compile-time checked) + dynamic SQL for runtime | Static for metadata, dynamic for business data |
| Migrations | SQLx / Refinery | Versioned, repeatable |
| Caching | Redis (with `deadpool-redis`) | Metadata cache, sessions, pub/sub |
| AuthN | `jsonwebtoken`, `argon2`, OAuth2 (`openidconnect`) | JWT + refresh tokens |
| Serialization | `serde` + `serde_json` | Everywhere |
| Validation | `validator` + custom expression engine | |
| Async jobs | **`apalis`** or `tokio-cron-scheduler` + Redis | Cron, retries, delayed |
| Search | PostgreSQL FTS → OpenSearch later | Start simple |
| Frontend | **Leptos** (CSR/SSR, WASM) *or* React + TypeScript | See §8 |
| Frontend build | Trunk (WASM) / Vite | |
| Observability | `tracing` + OpenTelemetry → Grafana/Loki/Tempo | |
| Config | `config-rs` / figment | Env + files |
| Containerization | Docker + docker-compose | Dev + prod |
| CI/CD | GitHub Actions | |

### Alternative considered: SeaORM
SeaORM offers nicer ergonomics but obscures the dynamic SQL we need for runtime data access. **SQLx with hand-written queries for metadata + a query-builder module for dynamic data** is the recommendation.

---

## 4. The Meta-Meta-Model (the heart of the system)

This is the schema that *describes the descriptions*. Everything below is a table in the `meta` schema.

### 4.1 Modules & Entities
```
md_module        - a logical grouping (e.g. "CRM", "HR")
  ├─ md_entity   - a business object definition (e.g. "Customer", "Invoice")
  │    ├─ md_field           - attribute definitions (type, validation, ui hints)
  │    ├─ md_relationship    - references between entities (1:N, N:N)
  │    ├─ md_constraint      - uniqueness, check, etc.
  │    ├─ md_index           - declared indexes
  │    └─ md_entity_state    - state-machine states (for workflows)
```

### 4.2 UI definitions
```
md_form          - create/edit layout for an entity
  └─ md_form_section / md_form_field  - tabs, groups, field placement, behavior
md_view (screen) - list view, calendar, kanban, detail, dashboard
  └─ md_view_column / md_view_filter / md_view_action
md_dashboard     - composite screens with widgets
  └─ md_widget
md_navigation    - menus, app switcher
```

### 4.3 Logic definitions
```
md_workflow      - state machine over an entity
  ├─ md_workflow_state
  ├─ md_workflow_transition  - guards, side effects, assignments
  └─ md_workflow_task        - user tasks / approvals
md_rule (business rule) - trigger (CRUD/timer/signal) + condition + action
md_expression    - reusable expression AST (DSL) for conditions/calculations
md_script        - sandboxed scripts for complex logic (optional)
```

### 4.4 Reporting
```
md_report        - data source + layout
  ├─ md_report_dataset   - query + params + joins
  ├─ md_report_grouping
  └─ md_report_chart     - bar/line/pie/table params
md_report_schedule - cron + recipients + format (pdf/xlsx/csv)
```

### 4.5 Security
```
sec_user / sec_group / sec_team              -- principals & org structure
sec_role                                     -- named bundle of privileges (additive; deny-by-default)
sec_role_assignment(user, role, scope?)      -- grant role to user/team (scope = team/BU for scoped roles)
sec_permission(role, entity, verb)           -- object-level CRUD: read|create|update|delete
sec_field_permission(role, entity, field, access)  -- field-level: none|read|write
sec_action_permission(role, action_id)       -- action-level: workflow transitions, custom actions
sec_field_constraint(entity, field, cond, msg) -- per-role write value constraints (ABAC)
sec_owd(entity, default)                     -- org-wide default: private|team|public_read|public_read_write
sec_role_hierarchy                           -- role tree (optional; "see records below me")
sec_share(record, principal, access)         -- explicit/manual record share
sec_share_rule(entity, cond, principal, access)  -- criteria-based auto-share (ABAC)
sec_record_share(record, principal, access)  -- MATERIALIZED computed shares (recalc on write/rule change)
sec_policy                                   -- general ABAC policies (expression engine)
```

### 4.6 Integration
```
int_connector    - typed adapter (REST/SOAP/DB/file/GraphQL/mq)
  └─ int_endpoint
int_mapping      - field mapping between external & internal entities
int_flow         - inbound/outbound ETL pipelines
  └─ int_flow_step
int_schedule
int_webhook      - outbound event subscriptions
```

### 4.7 System
```
sys_audit_log    - every write, who/when/what (before/after JSONB)
sys_outbox       - transactional outbox: durable pending side-effects (webhook/email/ETL/event),
                   inserted in the same txn as the data write; drained by workers (at-least-once). §5.9
sys_event_log    - canonical sequence-numbered domain-event stream: real-time + audit + replay (§5.10)
sys_lock         - soft advisory record checkout (owner, ttl, heartbeat) for UX coordination. §5.9
sys_setting      - key/value config
sys_translation  - i18n strings (metadata & data)
sys_version      - metadata versioning / migration tracking
```

### 4.8 Model lifecycle & versioning
The **active model** lives in the normalized `md_*` tables above — that is what the runtime reads on the hot path. Drafts and history are managed by these tables (semantics in §5.8):

```
md_active_version - per tenant: (tenant_id, version, snapshot_id) — pointer to the live model
md_draft          - editable model: (id, tenant_id, name, parent_snapshot_id,
                    model JSONB, status[draft|validating|publishing|published|failed],
                    editor_user_id, version_etag, created_at, updated_at)
md_snapshot       - immutable archive: (id, tenant_id, version, model JSONB,
                    manifest JSONB[change summary], created_by, created_at)
md_migration_log  - publish execution log: (id, draft_id, op, status, rows_affected,
                    started_at, finished_at) — for resume/revert
md_retirement     - pending two-phase deletes: (id, tenant_id, kind, target_id,
                    retired_at, purge_after, status[retired|purged])
```

---

## 5. Key Design Decisions to Make Early

### 5.1 ⭐ Dynamic data storage strategy

**Decision (recorded): Pattern B — real table per entity with hoisted relational columns + native Postgres FK constraints, JSONB only for the non-relational remainder.**

This is the Odoo / Microsoft Dataverse model, chosen over Salesforce's universal-table approach because we do **not** have a multitenancy constraint that would force app-layer RI. Native constraints are stronger, faster, and free; we pay for them with a DDL/migration engine applied at publish time — which we need anyway for data migration on schema change (REVIEW.md C3). See `docs/ri-strategies.md` for the full survey and rationale.

**The spectrum (for context):**

| Pattern | Storage | RI mechanism | Examples | Verdict |
|---|---|---|---|---|
| **A. Universal/shared table** | Wide tables + KV overflow | App-layer only | Salesforce | ❌ Only justified by extreme multitenancy |
| **B. Real table/entity + native FK** | Real table per entity | DB `FOREIGN KEY` | Odoo, Dataverse | ✅ **Chosen** |
| **C. Real table/entity + soft refs** | Real table per entity | App-layer, GUID cols | ServiceNow | Middle ground; weaker RI than B |
| **D. JSONB document** | JSONB payload per entity | App-layer (or hoisted cols) | many modern apps | ⚠️ Inherits A/C's RI obligation — avoid as the sole strategy |

**Concrete storage model for MDA:** each entity publishes to one real table `biz.<entity_table_name>` with —

| Column group | Columns | Notes |
|---|---|---|
| **Core (every table)** | `id ULID PK`, `tenant_id`, `owner_id`, `state`, `version BIGINT`, `created_at`, `updated_at`, `deleted_at` | Fixed across all entities; `tenant_id` in every composite index + RLS policy; `version` drives optimistic concurrency (§5.9) |
| **Hoisted relational** | one real typed column per reference field, e.g. `ref_customer_id ULID` | Real `FOREIGN KEY ... ON DELETE <behavior>`; `DEFERRABLE INITIALLY DEFERRED` for mutual references |
| **Hoisted scalar (optional)** | real columns for indexed/unique/hot fields, e.g. `email TEXT UNIQUE` | Hoist from JSONB automatically via **generated columns**: `email TEXT GENERATED ALWAYS AS (attributes->>'email') STORED` |
| **Flexible payload** | `attributes JSONB` | Everything else; GIN-indexed for ad-hoc query |

**Rules:**
- A reference field is **always** hoisted to a real column with a real FK — never stored only in JSONB. Non-negotiable; this is the whole point of choosing Pattern B.
- Plain scalars default to JSONB; promote to a real column (or generated column) when they need an index, a unique constraint, or heavy filter load.
- Indexes (incl. unique) on JSONB values use expression or generated-column indexes.
- Table DDL is generated and applied **at publish time** by the DDL/migration engine, never ad hoc at runtime.

**Why not pure JSONB (Pattern D)?** A value inside JSONB cannot carry a real FK, so RI would have to be enforced in the application layer — re-implementing existence checks, cascade rules, orphan sweeps, and race handling that Postgres already provides correctly. See `docs/ri-strategies.md` §3 for what that costs. Not worth it without Salesforce's justification.

### 5.2 Expression language (DSL)
Business rules, validations, workflow guards, report filters all need expressions ("`amount > 1000 AND status == 'open'`").
- **Option 1:** Embed a scripting language (Rhpr / Rune / Boa) — powerful, slower, sandbox risk.
- **Option 2:** Build a small typed expression AST evaluator in Rust (like `eval` crates or custom).
- **Recommendation:** **Custom JSON AST evaluator** stored as JSONB, evaluated by a Rust interpreter. Safe, serializable, fast, testable. Add a `rune` escape hatch later for power users behind a capability flag.

### 5.3 Hot reload of metadata
Metadata changes must take effect without restart:
- Cache metadata in a read-through in-memory cache (`moka`).
- Invalidate via PostgreSQL `LISTEN/NOTIFY` on a `meta_changed` channel, or Redis pub/sub.
- Version-stamp every metadata read so runtime can detect staleness.

### 5.4 Multi-tenancy
- **Strategy A:** `tenant_id` column on every table + app-enforced filter. Simple, shared-everything.
- **Strategy B:** Schema-per-tenant in PostgreSQL. Strong isolation, more ops.
- **Recommendation v1:** **Strategy A** with PostgreSQL **Row-Level Security** enforcing tenant isolation at the DB layer (defense in depth). Make tenant_id part of all composite indexes.

### 5.5 Versioning & migrations of metadata
- Metadata is deployable like code. Export/import as JSON bundles.
- Track `md_version` snapshots. Support "promote model from dev → staging → prod" via import.
- Always backward-compatible additive changes; destructive changes require a migration job.

### 5.6 Extensibility model
- **Field types:** registry of built-in types + a Rust trait `FieldType` that plugins implement (compiled into the binary via a registry pattern; dynamic loading via `wasmtime` later).
- **Functions:** the expression DSL calls registered Rust functions (e.g., `now()`, `sum()`, custom).
- **Connectors:** trait `Connector` with HTTP/DB/file/generic implementations.
- **Webhooks:** outbound HTTP on domain events.

### 5.7 Relationship modeling & cascade behaviors

Relationships are data (`md_relationship`) and carry **strength** and **delete behavior** — mirroring Salesforce's master-detail/lookup split and Dataverse's behavior matrix. This metadata drives the FK DDL emitted at publish (§5.1).

**Relationship strength:**

| Strength | Semantics | FK DDL | Enables |
|---|---|---|---|
| **master_detail** | Parent owns child; child cannot exist without parent; ownership & sharing inherited | `NOT NULL` column + `ON DELETE CASCADE` | Roll-up summary fields (sum/count of children on the parent) |
| **lookup** | Loose, optional reference | nullable column + `ON DELETE <behavior>` | Reference qualifiers (constrain valid targets) |

**`on_delete` behavior for lookups** (metadata field → FK clause):

| Value | Meaning | FK clause |
|---|---|---|
| `restrict` | Block parent delete while referenced | `ON DELETE RESTRICT` (or `NO ACTION`) |
| `set_null` | Clear the reference | `ON DELETE SET NULL` (column must be nullable) |
| `cascade` | Delete the referencing records too | `ON DELETE CASCADE` |

**`md_relationship` columns (refined from §4.1):**

```
md_relationship
  id, tenant_id, name, label
  source_entity_id        -- the entity holding the reference
  source_field_name       -- the hoisted column name, e.g. ref_customer_id
  target_entity_id        -- the referenced entity
  cardinality             -- one_to_many | many_to_one | many_to_many
  strength                -- master_detail | lookup
  on_delete               -- restrict | set_null | cascade   (lookup only)
  required                -- bool (master_detail always true)
  reference_qualifier     -- optional expression restricting valid targets
  rollup_summary          -- optional: aggregate children onto parent
                          --   (count/sum/avg/min/max of a field)
```

**Many-to-many** is materialized as a real **join table** `biz.<a>_<b>` with a composite PK and FKs to both sides — real constraints, real integrity.

**Mutual references** (A references B, B references A) use `DEFERRABLE INITIALLY DEFERRED` FKs so integrity is checked at commit, not per statement.

**Soft-delete interaction (decide in Phase 0):** Soft-delete (`deleted_at`) **defeats native cascade** because the row isn't actually deleted, so `ON DELETE CASCADE` never fires. Two coherent choices:

- **Hard-delete + archive (recommended):** move the row to `biz_archive.<table>` on delete; native FKs cascade naturally; the archive gives recoverability without a cascade-complexity tax. Keeps RI trustworthy end to end.
- **Soft-delete + app-layer cascade:** runtime walks inbound relationships and applies `on_delete` on the logical delete. Re-implements cascade in app code (the ServiceNow/Salesforce route).

> Recommendation: **hard-delete + archive table.** It preserves native RI end to end and the archive gives recoverability.

### 5.8 Metadata lifecycle: draft → validate → publish → activate

**Problem being solved (REVIEW.md C2, enables C3):** metadata describes data that already exists and that other metadata depends on. You cannot freely mutate live metadata — it must be validated, may require DDL + data migration, must be previewable before activation, and must be rollbackable. Therefore **all edits go through drafts; publish is the only path to activation.** There is no "edit the active model directly."

**Three states:**
- **Active** — the live, published model. The `md_*` tables *are* the active model; runtime reads them directly (fast, simple hot path).
- **Draft** — an editable, in-progress model stored as a JSONB document (`md_draft`). Not visible to runtime.
- **Snapshot** — an immutable archive of a prior active model (`md_snapshot`), for history, diffing, and rollback.

**Lifecycle:**
1. **Branch** — create a draft from the current active model (or from a snapshot).
2. **Edit** — Studio mutates the draft JSONB. v1: one editor per draft (checkout lock + optimistic `version_etag`); multi-editor collaboration is a v2 extension.
3. **Validate** — server runs dependency/integrity checks and produces a **migration plan** (dry-run). Nothing is applied.
4. **Preview/Test** — Studio renders forms/views/reports against the draft (loaded into an ephemeral cache), optionally against a scratch dataset.
5. **Publish** — apply the migration plan atomically, promote draft → active, archive previous active → snapshot.
6. **Rollback** — restore a prior snapshot (re-published as a draft → publish).

**Migration plan = diff(active, draft) → ordered, classified op list.** The same op list drives changes to *both* (a) the `biz` data schema/data and (b) the `md_*` metadata tables — one diff, two effects.

| Op class | Examples | Behavior |
|---|---|---|
| **Additive (safe)** | add entity, add nullable field, add nullable relationship | Apply immediately; no data risk |
| **Transforming** | field type change, make required, rename field/entity | Generate data transform (cast + validate + on-failure policy); batched for large tables |
| **Destructive (two-phase)** | drop field, drop entity, drop relationship | Phase 1 **retire** (mark inactive, hide, keep data); Phase 2 **purge** after grace (default 14 days) with explicit confirmation |

**Validation checks at publish:**
- No dangling references (relationship target entity/field exists)
- No orphaned dependencies (forms/views/rules/reports referencing deleted fields/entities)
- Type-compatibility for transforms (e.g. every value must parse as the target type)
- FK cycle detection → mark `DEFERRABLE` (§5.7)
- Reserved-name / duplicate-name collisions
- Row-count estimate for transforming ops (warn on large tables)

**Publish execution (transactional, one-at-a-time per tenant via `pg_advisory_lock`):**
1. Re-validate (draft may have changed since dry-run)
2. Begin transaction; run additive + transforming DDL + data migrations (batched; each step logged to `md_migration_log` for resume/revert)
3. Apply the diff to `md_*` metadata tables
4. Archive previous active model to `md_snapshot` (JSONB + manifest)
5. Bump `md_active_version`
6. Commit; broadcast `meta_changed` (LISTEN/NOTIFY + Redis) to invalidate caches
7. On failure → rollback transaction; `md_migration_log` enables targeted resume

**Two-phase destructive deletes:**
- **Retire** — `status = retired`. Data preserved; runtime & UI hide it; queries exclude it. Fully reversible (un-retire).
- **Purge** — scheduled job after grace drops the column/table for real. **Irreversible**; blocked if any non-retired dependency still references it.

**Rollback:** keep last N snapshots (default 10). Rollback loads the snapshot as a draft and re-publishes (reverse migration). **Caveat:** rollback cannot restore data already purged by a two-phase destructive op; retire-phase changes are fully reversible.

### 5.9 Concurrency & transactional semantics

(Resolves REVIEW.md **C4**.) The system must (a) prevent lost updates, (b) keep multi-step writes atomic, and (c) guarantee that external side-effects are delivered exactly without coupling request latency to external systems. The answers below are deliberately standard patterns — do not invent novel concurrency.

**1. Record-level concurrency: optimistic by default.**
- Every `biz.<table>` carries a `version BIGINT` (core column, §5.1). Updates are conditional:
  `UPDATE … SET …, version = version + 1 WHERE id = $1 AND version = $expected`
- 0 rows affected → another writer committed first → API returns **409 Conflict**; client re-reads and retries. Surfaced over HTTP via `ETag`/`If-Match` (and a `version` field in the body for non-browser clients).
- List responses include per-row versions so clients can echo them back.
- Postgres MVCC (READ COMMITTED) means readers never block writers; OCC is the layer that prevents *lost updates* on top of that.

**2. Soft checkout (`sys_lock`) is advisory UX, not hard correctness.**
- Optional "record is being edited by Alice" lock with TTL + heartbeat — shows a banner, prevents duplicated effort, auto-releases on timeout or explicit release.
- It does **not** enforce correctness; OCC does. Hard pessimistic `SELECT … FOR UPDATE` is reserved for genuine serialization (workflow/timers, below), never routine edits.

**3. The transactional unit of work for a write.** A single request that mutates a record runs this ordered sequence in **one DB transaction**:
1. Load record (`FOR UPDATE` only if a workflow transition or timer is involved)
2. AuthZ check (RBAC + ABAC)
3. **Before-rules** (validate / mutate input) — may **reject** → rollback
4. Apply field changes (incl. workflow `state`)
5. **After-rules, synchronous** — data side-effects (set fields, update related rows) within the same txn; failure rolls back the whole write
6. Insert `sys_event_log` rows (canonical domain events — §5.10) and `sys_outbox` rows (async side-effects: email, webhook, ETL kick)
7. `UPDATE … version + 1` (OCC)
8. Commit
9. *(Workers drain `sys_outbox` — see §5.9.4)*

**4. Transactional Outbox pattern (the key decision).** External/integration side-effects (webhook, email, pub/sub, scheduled kicks) are **never** called inside the data transaction. Instead:
- The data change and an `sys_outbox` row are inserted in the **same transaction** → the dual-write problem is eliminated: *if the data committed, the side-effect is durably queued.*
- A worker claims rows with `SELECT … FOR UPDATE SKIP LOCKED`, performs the external call, retries with exponential backoff + jitter, and routes persistent failures to a **dead-letter** set for manual replay.
- Delivery is **at-least-once**; therefore all consumers must be **idempotent** (stable message id; webhooks carry an idempotency header; internal processors dedupe via a processed-id log).

This yields a clean, answerable rule for "are rules/workflows trustworthy or eventual?":
- **Data-affecting logic = trustworthy** (synchronous, in-transaction, atomic with the write).
- **Notification/integration side-effects = eventual** (durable via outbox, at-least-once).

**5. Rule & workflow execution model.**
- Rules fire **synchronously within the write transaction** for data effects; async-only side-effects go to the outbox.
- Multiple rules matching one event are ordered **deterministically: `priority` then `id`**.
- **Recursion budget:** a synchronous side-effect that re-triggers after-rules is capped (default depth 10); exceeding it aborts the transaction. Guards against rule loops (ties to expression-engine limits, REVIEW.md U6).
- A workflow transition is a specialized update: guards evaluated → `state` set → on-transition actions run in-transaction → notifications to outbox. A transition that should trigger *another* transition emits a domain event to the outbox rather than chaining synchronously — keeping each transaction bounded and avoiding cascade locks.

**6. Workflow timers & concurrent transitions → pessimistic serialization.**
- This is the one place hard row locking is required: a scheduled timer (SLA escalation, due transition) must not fire concurrently with a user-initiated transition. The worker loads the record `FOR UPDATE`, checks current state, then proceeds or aborts.
- Timers are purely async (job queue); on fire they run the same unit-of-work sequence as a user transition.

**7. Isolation, lock ordering, deadlocks.**
- **Isolation: READ COMMITTED** (Postgres default) + explicit locks where needed. Do not globally raise to SERIALIZABLE (perf cost + serialization-failure retry complexity).
- **Lock ordering:** acquire locks in a deterministic order (parent before child, ascending by id) to prevent deadlocks across multi-row transactions.
- **Deadlock handling:** on SQLSTATE `40P01` / `40001`, the app retries the unit of work with bounded backoff.
- Keep transactions short; push anything slow (external I/O) to the outbox.

### 5.10 Real-time channel for the runtime UI

(Resolves REVIEW.md **C5**.) A metadata-driven UI where another user edits the same record, or a workflow silently moves it, must push updates — otherwise the UX collapses into stale data and the OCC conflicts from §5.9 surface only as frustrating 409s on save. The channel is built on the canonical event stream already produced by every write (§5.9), so it adds almost no new state.

**1. Transport: SSE first.** Server-Sent Events for the server→client push that is ~90% of our traffic (record/task/workflow notifications, list refresh). Client→server (mutations, presence heartbeats) goes through the normal REST API. SSE auto-reconnects, is HTTP-friendly behind load balancers, and is far simpler to scale than WebSocket. Reserve WebSocket for a future collaborative-co-editing feature that needs low-latency bidirectional ops.

**2. Event source: `sys_event_log`.** The canonical, sequence-numbered, transactional domain-event stream (written in the same txn as the data change, §5.9.3 step 6). Real-time, audit, and replay all read from one place. Reconciling with §5.9.4: `sys_event_log` = the **facts** (what happened); `sys_outbox` = the **work items** (async delivery that needs a worker — webhook/email/ETL), referencing event-log rows. The relay reads `sys_event_log`; workers drain `sys_outbox`.

```
sys_event_log
  seq BIGINT PK  -- per-tenant monotonic; doubles as the SSE Last-Event-ID
  tenant_id, ts
  type           -- record.created | record.updated | record.deleted
                 -- workflow.transitioned | task.assigned | task.completed
                 -- record.checked_out | record.released | metadata.published | notification.*
  entity, record_id
  payload JSONB  -- changed_fields, from_version, to_version, actor
  -- indexes: (tenant_id, seq), (tenant_id, entity, record_id)
  -- retention: ~7-30 days for replay; older rows partitioned/archived
```

**3. Subscription model (channels).** Clients subscribe to topics; the relay keeps an in-memory `channel → client set` map:
- `entity:<name>` — all changes to an entity (live lists / kanban / counts)
- `record:<entity>:<id>` — a specific record (detail views, conflict detection)
- `user:<id>:tasks` — task assignments/completions (inbox badge)
- `user:<id>:notifications` — personal notifications
- `tenant:<id>:broadcast` — system-wide (incl. `metadata.published` → UI reloads model, tying into cache invalidation §5.3)

**4. Relay & fan-out.** Each app instance runs a relay holding its locally-connected SSE clients. On a new `sys_event_log` row:
- A trigger fires `NOTIFY mda_event` (payload = seq); each instance's background `LISTEN` reads the row and fans out to local clients by channel.
- Postgres NOTIFY fans out to all LISTENing instances across the cluster, so **no Redis is required for the core DB→app hop**.
- Scale path: if NOTIFY volume becomes a bottleneck, front it with Redis Pub/Sub (or a stream) as the cross-instance bus; the SSE clients and `sys_event_log` contract stay unchanged.

**5. Reliability: `Last-Event-ID` replay.** SSE clients send `Last-Event-ID` (= last `seq` seen) on (re)connect. The server replays `sys_event_log WHERE seq > $last AND matches-subscription`, then switches to live. Result: **at-least-once delivery to the client within the retention window** — no missed events across reconnects.

**6. AuthZ on the channel (critical).** A client must only receive events for records/fields it is authorized to see:
- Authenticate the SSE connection (JWT).
- Authorize each subscription (can this user see this entity/record/view?).
- The relay filters events per client using the same RBAC+ABAC+data filters as the REST API (§5.6, C6) — including **field-level visibility** (never leak a change to a masked field). Access decisions are cached to keep this cheap.

**7. Client merge strategy (ties to OCC §5.9).** On receiving `record.updated` for the record the client is viewing:
- Not editing → refresh the view.
- Editing (unsaved changes) and `to_version` advanced → show a conflict banner ("changed by someone else — Review / Overwrite / Refresh") *before* the user wastes effort. The 409-on-save remains the backstop. This is the UX payoff of combining OCC + real-time.

**8. Presence (lightweight).** "Who else is viewing/editing this?" — clients heartbeat `POST /api/presence/:entity/:id` (~15s); the server tracks in Redis (TTL) keyed by (record, user) and broadcasts `record.checked_out`/presence deltas over the channel. This is the view-level complement to the explicit edit-level soft checkout (`sys_lock`, §5.9.2).

**9. v1 scope.** In Phase 6 (with the Runtime UI): SSE channel + reliable replay + conflict banner — the minimum to avoid stale-data UX. Deferred: live list/kanban/count streaming, presence, and collaborative co-editing (the last needs WebSocket + OT/CRDT → v2).

### 5.11 Authorization — multi-grained, deny-by-default, ABAC-powered

(Resolves REVIEW.md **C6**.) Enterprise authorization operates at six grains; the original `sec_permission` ("verb on entity") covered only one. The model below covers all six, is **deny-by-default** with additive grants (Salesforce-style — no negative permissions, simpler reasoning), and uses the expression engine (§5.2 / Phase 4) as the ABAC evaluator.

**The six grains:**

| # | Grain | Question answered |
|---|---|---|
| 1 | **Tenant** | Is this user in the right tenant? |
| 2 | **Object (entity)** | Can the user CRUD this entity at all? |
| 3 | **Record (row)** | Which specific rows can they see/write? |
| 4 | **Field (column)** | Which fields can they read / write? |
| 5 | **Action / transition** | Can they invoke this workflow transition / custom action? |
| 6 | **Value (write constraint)** | What values may they write to a field? |

**1–2. Tenant + object level.** Tenant isolation via Postgres RLS on `tenant_id` (§5.4) — hard, always-on, defense-in-depth. Object CRUD via `sec_permission(role, entity, verb ∈ {read,create,update,delete})`, checked at the service boundary.

**3. Record level (row security) — deny-by-default, then grant.** The hardest grain; the principle is "start restrictive, open up":
- **Baseline = OWD** (`sec_owd`): per entity, default ∈ `{private, team, public_read, public_read_write}`. *Private* = see only your own.
- **Ownership**: the record owner always has R/W.
- **Team/group**: records owned by a team the user belongs to are visible per OWD.
- **Sharing rules** (`sec_share_rule`): criteria-based auto-shares when a record matches an ABAC condition (e.g. "region=West → share with West team").
- **Manual shares** (`sec_share`): explicit record→principal grants.
- **Role hierarchy** (`sec_role_hierarchy`, optional): a user can see records owned by users below them in the hierarchy.

**Critical split — where row security runs:**
- **Postgres RLS = tenant isolation only** (cheap, simple, hard boundary). Do *not* encode the full sharing model in RLS — ABAC conditions, team membership, and recursive hierarchies make RLS policies brittle and slow.
- **App layer (`mda-data`) = business row security.** The query builder injects a predicate derived from the user's effective context (owner / teams / shares / rules). This must be **query rewrite** (a WHERE clause), never post-filtering — post-filtering leaks counts and paginates wrong.

**Performance — materialized sharing.** Computing "can U see R?" live is expensive (ownership + team + shares + rules + hierarchy). Maintain a denormalized **`sec_record_share(record_id, principal_id, access)`** table, recalculated when a record is written or a sharing rule changes (Salesforce's "sharing recalculation"). List queries join against it. Rule/hierarchy evaluation is pushed into this table asynchronously (via the outbox, §5.9) so writes stay fast.

**4. Field level (FLS).** `sec_field_permission(role, entity, field, access ∈ {none, read, write})`:
- `none` → hidden: dropped from read responses, rejected on write.
- `read` → returned but read-only; rejected on write.
- `write` → fully editable.
- Enforced in the **serialization layer** (read projection) and the **deserialization/mutate layer** (write rejection).

**5. Action / transition level.** `sec_action_permission(role, action_id)` gates workflow transitions and custom actions — checked at the action invocation boundary. Modeling transitions as explicit actions (not just "update") is what lets you say "only the Approve role can run the Approve transition."

**6. Value constraints (write ABAC).** `sec_field_constraint(entity, field, condition, message)` — per-role conditions evaluated by the expression engine at write time (e.g. "role=sales_rep ⇒ discount ≤ 0.05"). **Distinct from validations** (`md_rule` / field validations): validations are universal data-correctness rules; field constraints are authorization-scoped ("*who* may set *what*"). Both use the same engine.

**Effective-context caching.** At session start, compute the user's effective context — roles (direct + via teams/groups), teams, role-hierarchy ancestors, and compiled sharing-rule predicates — and cache it (Redis, TTL) keyed by `user_id`; invalidate on role/team/sharing-rule change. Every grain consults this context rather than re-querying the `sec_*` tables per request.

**Enforcement map (the key artifact):**

| Grain | Enforced at | Mechanism |
|---|---|---|
| Tenant | DB (always) | Postgres RLS on `tenant_id` (§5.4) |
| Object (entity CRUD) | Service boundary | `sec_permission` lookup |
| Record (read/list) | Query builder (`mda-data`) | Predicate injection from effective context; deny-by-default from OWD |
| Record (write) | Before mutate | Check the specific record against the policy |
| Field (read) | Serialization layer | Project: drop `none`, expose `read`/`write` |
| Field (write) | Deserialization / mutate | Reject writes to non-`write` fields |
| Field value | Write validation | Expression engine (ABAC) |
| Action / transition | Action boundary | `sec_action_permission` |
| Event channel (C5) | SSE relay | Per-client row filter + FLS on `sys_event_log` payloads |

**No negative permissions.** The model is purely additive on top of a deny-by-default baseline (matches Salesforce; avoids deny-vs-grant precedence ambiguity). For an exception, narrow the role or add a more specific OWD/sharing rule.

---

## 6. Component Breakdown (crate / module level)

Use a **Cargo workspace** to keep boundaries clean.

```
mda/
├── Cargo.toml                 # workspace
├── crates/
│   ├── mda-core/              # shared types, errors, Result, ids, traits
│   ├── mda-meta/              # metadata model structs + loader + cache
│   ├── mda-data/              # dynamic data access (query builder, CRUD over JSONB)
│   ├── mda-expression/        # DSL AST + evaluator + functions registry
│   ├── mda-security/          # authN, authZ (RBAC+ABAC), data filters
│   ├── mda-workflow/          # state machine engine + timers
│   ├── mda-rules/             # business rule engine (triggers/conditions/actions)
│   ├── mda-reports/           # report dataset builder + renderers (pdf/xlsx/csv)
│   ├── mda-integration/       # connectors, mappings, ETL flows
│   ├── mda-audit/             # audit/event logging
│   ├── mda-api/               # HTTP handlers (Axum) — the "edge"
│   └── mda-server/            # binary: wires everything, config, bootstrap
├── migrations/                # SQLx migrations for meta schema
├── web/                       # frontend (Leptos or React)
├── docker/
├── docs/
└── tests/                     # end-to-end + integration
```

### Module responsibilities (detail)

- **mda-meta** — Load `md_*` tables into typed structs; expose `MetadataCache` (query by entity id, invalidate on change). Defines the canonical `Entity`, `Field`, `Form`, `View`, etc. types used everywhere.
- **mda-data** — Given an `Entity` and an operation (create/read/update/delete/list), produce the correct SQL against `biz.<table>`. Query builder for list views (filters/sort/paging over JSONB). Handles validation, defaults, computed fields.
- **mda-expression** — Parse/evaluate the DSL. Inputs: expression AST (JSON), record context, function registry. Returns typed values. Used by rules, workflows, validations, reports.
- **mda-security** — `Identity` (user/tenant/roles/teams); multi-grained `check` (object / field / record / action, §5.11); ABAC via the expression engine; injects record-level predicates into `mda-data` queries; tenant isolation via Postgres RLS (§5.4).
- **mda-workflow** — State machine: given entity + current state + transition request, evaluate guards (expressions), execute actions, persist new state, enqueue tasks/notifications.
- **mda-rules** — Triggers: before/after CRUD, on-event, on-schedule. Sequence: match → condition → action. Actions: set field, call function, fire event, send webhook, enqueue.
- **mda-reports** — Build dataset (run parameterized query against data layer), apply grouping/aggregation, render to table/chart/pdf/xlsx.
- **mda-integration** — `Connector` trait; flows pull/push records with field mapping; scheduled via job queue; idempotency keys.
- **mda-api** — REST (OpenAPI via `utoipa`) + optional GraphQL (async-graphql). Routes map to service layer. Studio API (CRUD on metadata) vs Runtime API (CRUD on business data) — same engine, different authz.
- **mda-server** — `main.rs`: load config, init DB pool, warm metadata cache, mount routers, start workers, graceful shutdown.

---

## 7. API Surface (high level)

### Studio API (build-time, admin only)
All edits target a **draft**, never the active model directly (§5.8). Publishing is the only path to activation.
```
# Draft lifecycle
POST   /api/studio/drafts                      # branch from active (or a snapshot)
GET    /api/studio/drafts/:id                  # read draft model (JSONB)
PATCH  /api/studio/drafts/:id                  # edit (If-Match etag → optimistic concurrency)
POST   /api/studio/drafts/:id/checkout         # lock for single-editor editing (v1)
POST   /api/studio/drafts/:id/validate         # dry-run → migration plan + validation report
POST   /api/studio/drafts/:id/preview          # load draft into ephemeral cache for Studio preview
POST   /api/studio/drafts/:id/publish          # apply migration + activate atomically
# Within a draft: define entities, fields, forms, views, workflows, reports, rules…
#   e.g. POST /api/studio/drafts/:id/entities, PATCH …/fields   (all draft-scoped)
# History & rollback
GET    /api/studio/snapshots                   # list published versions
POST   /api/studio/snapshots/:id/rollback      # restore a prior version (re-publish)
# Bundle transport (dev → staging → prod)
GET    /api/studio/export?snapshot=:id         # export model bundle (JSON)
POST   /api/studio/import                      # import bundle as a new draft
```

### Runtime API (use-time, end users)
```
# Generic, dynamic — entity name is a path param
GET    /api/data/:entity?filter=...&sort=...&page=...   # list
GET    /api/data/:entity/:id                            # read
POST   /api/data/:entity                                # create
PATCH  /api/data/:entity/:id                            # update
DELETE /api/data/:entity/:id                            # soft delete
POST   /api/data/:entity/:id/:transition                # workflow action
GET    /api/forms/:entity                               # form definition (for UI)
GET    /api/views/:entity                               # list-view definition
GET    /api/reports/:id/run?params=...                  # run report
GET    /api/dashboards/:id                              # dashboard widgets + data
```

### Auth
```
POST   /api/auth/login      POST /api/auth/refresh
GET    /api/auth/me
```

> OpenAPI spec auto-generated; an external SDK (TypeScript/Rust/Python) can be derived from it. The dynamic data API is discoverable via `/api/schema/:entity` (JSON Schema derived from metadata).

---

## 8. Frontend Strategy

**Decision: Rust WASM (Leptos) vs. React/TypeScript.**

| | Leptos (Rust/WASM) | React + TS |
|---|---|---|
| Type sharing with backend | Excellent (share serde types) | Codegen (ts-rs) |
| Ecosystem / component libs | Growing, thinner | Huge |
| Hiring / contribution | Niche | Mainstream |
| Full-stack Rust story | ✅ complete | ❌ split stack |
| Studio UI complexity | Doable, more work | Faster to ship |

**Recommendation:**
- **Runtime UI (end users):** Must render from metadata dynamically. Either works; **Leptos** keeps the "all Rust" promise and shares types natively. A metadata-driven form/table renderer is mostly logic, not visual flair.
- **Studio UI (admin):** Complex, drag-and-drop form/view/report designers. Consider **React + TS** here if Leptos component ecosystem is insufficient, OR invest in Leptos. Start in Leptos and reassess at Phase 8.

The runtime UI is a **metadata interpreter**: fetch form/view JSON → render inputs/tables → POST back. It is thin; the server is the brain.

---

## 9. Development Roadmap (Phased)

Each phase is independently valuable and demoable. Aim for vertical slices.

### Phase 0 — Foundation (Weeks 1–3)
- Cargo workspace skeleton, CI, Docker, dev Postgres+Redis
- `mda-core`: error types, IDs (`ulid`), traits
- Config, logging (`tracing`), health endpoint
- SQLx setup + migration runner; `meta` schema skeleton
- **Deliverable:** `cargo run` boots, `/health` responds, migrations run.

### Phase 1 — Metadata Engine (Weeks 3–6)
- Define `md_module`, `md_entity`, `md_field`, `md_relationship` + lifecycle tables (`md_draft`, `md_snapshot`, `md_active_version`) per §4.8
- `mda-meta`: loader + in-memory cache (`moka`) + LISTEN/NOTIFY invalidation
- **Draft→publish lifecycle (§5.8):** all metadata edits go through drafts; publish applies the diff to `md_*` (and later `biz`). v1 supports **additive ops only** (transforming/destructive come in Phase 2)
- Studio API: draft branch / edit / validate / publish; export/import as JSON
- **Deliverable:** Branch a draft, add a "Customer" entity, validate, publish; cache reflects the new active model.

### Phase 2 — Dynamic Data Layer (Weeks 6–9)
- `mda-data`: **DDL/migration engine** — publish generates `biz.<table>` (core + hoisted relational/scalar columns + native FK constraints per §5.7) and classifies ops additive/transforming/destructive (§5.8)
- Transforming ops: data casts with on-failure policy, batched for large tables
- Two-phase destructive: retire now, purge after grace via scheduled job (`md_retirement`)
- Generic CRUD + list with filter/sort/paging (query over hoisted columns + JSONB)
- Field type registry: string, number, bool, date, enum, reference, json
- Reference fields → real typed columns with native `FOREIGN KEY` (per §5.7)
- Validations + defaults
- **Optimistic concurrency (§5.9):** `version` column + `If-Match`/ETag on PATCH → 409 on conflict
- `/api/data/:entity/*` runtime routes
- **Deliverable:** Create/read/update Customer records via API, fully driven by metadata, with real FK-enforced relationships; adding/renaming/dropping a field runs a validated migration.

### Phase 3 — Security & Auth (Weeks 9–11)
- Users, teams, roles (`sec_*`); JWT auth + refresh tokens
- **Object-level RBAC** (`sec_permission`) on every route
- **Field-level security** (`sec_field_permission`: none/read/write) — enforced in serialization + write
- **Record-level**: ownership + team baseline (`sec_owd`), app-layer predicate injection in `mda-data`; tenant isolation via Postgres RLS (§5.4)
- Effective-context caching (roles/teams) per session
- Audit logging (`sys_audit_log`) on all writes
- *Deferred within v1:* criteria-based sharing rules, role hierarchy, materialized `sec_record_share` (§5.11.3)
- **Deliverable:** Login; role-gated object + field + record access; tenant isolation; full audit trail.

### Phase 4 — Expression Engine & Rules (Weeks 11–14)
- `mda-expression`: AST types, parser (from JSON), evaluator, function registry
- `mda-rules`: before/after CRUD triggers, conditions, basic actions
- Validations & calculated fields now use the engine
- ABAC policies use the engine
- **Transactional model (§5.9):** data-affecting rules run synchronous & atomic in the write txn; async side-effects go to the **transactional outbox** (`sys_outbox` + draining worker — at-least-once, idempotent consumers)
- **Deliverable:** Define a rule "when status changes to Closed, set closed_at = now()"; it fires.

### Phase 5 — Workflow Engine (Weeks 14–17)
- State machine over entities (`md_workflow*`)
- Transitions with guards (expressions) + side effects; on-transition actions run in-transaction (§5.9)
- User tasks / approvals + assignments
- **Async timers** (scheduled transitions / SLA escalation) via job queue; worker serializes with `SELECT … FOR UPDATE` against concurrent user transitions (§5.9.6)
- Notifications (email/webhook) on state changes via the transactional outbox
- **Deliverable:** An approval workflow on an "Invoice" entity with state, tasks, and email.

### Phase 6 — Form & View Definitions + Runtime UI (Weeks 17–22)
- `md_form`, `md_view`, `md_dashboard`, `md_navigation`
- `/api/forms/:entity`, `/api/views/:entity` return renderable JSON
- **Build the Runtime UI** (Leptos): dynamic form renderer, list/grid renderer, dashboard, navigation shell
- **Real-time channel (§5.10):** SSE over `sys_event_log` with `Last-Event-ID` replay; conflict banner when a viewed record is changed by another user
- **Deliverable:** A user logs in, sees a menu, opens a list of Customers, creates/edits via a rendered form, and is alerted in real time when another user edits the same record — all from metadata, zero hardcoded pages.

### Phase 7 — Reporting (Weeks 22–25)
- `md_report`, datasets, grouping, charts
- Renderers: HTML table, XLSX, PDF, CSV
- Scheduled reports via job queue + email delivery
- Dashboards consume report datasets
- **Deliverable:** Build a "Sales by Month" report in Studio, run it, export to PDF, schedule daily email.

### Phase 8 — Studio UI (Weeks 25–31)
- Entity/field designer, form designer (drag-drop), view designer, report designer, workflow designer, rule editor, security admin
- Metadata import/export/promote UI
- This is large; consider parallelizing across designers
- **Deliverable:** A business analyst can build a small CRM app entirely through the browser.

### Phase 9 — Integration Layer (Weeks 31–34)
- `Connector` trait: REST, DB, file, (SOAP/GraphQL later)
- `md_int_*`, field mapping, inbound/outbound flows, scheduling
- Webhooks (outbound) + inbound webhook receiver
- **Deliverable:** Sync Customer records to/from an external REST API on a schedule.

### Phase 10 — Hardening, Scale, Polish (Weeks 34–40)
- Observability (tracing/OpenTelemetry dashboards), load testing
- Metadata cache tuning, query optimization, materialized views for reports
- i18n (`sys_translation`), theming
- Backup/restore runbook, blue-green deploys
- Security review, pen-test, OWASP hardening
- Documentation, SDK generation, sample apps
- **Deliverable:** Production-ready v1.0.

> Timelines are rough planning anchors for a small team; adjust to reality. A solo dev should ~double these.

---

## 10. Testing Strategy

| Layer | Tooling | Focus |
|---|---|---|
| Unit | `cargo test` | Expression evaluator, query builder, state machine |
| Integration | `cargo test` + testcontainers (real Postgres/Redis) | Metadata CRUD, dynamic data, rules, workflows |
| E2E | `cargo test` HTTP + (optional) Playwright for UI | Full vertical: define model → use it |
| Property | `proptest` | Expression eval invariants, query builder |
| Load | `katoa`/`oha`/`wrk` | API throughput, metadata cache |
| Metadata regression | Snapshot golden tests | Model export format stability |

**Golden rule:** every Phase deliverable is backed by an E2E test that defines metadata via API and exercises the runtime against it. This is the only way to trust a metadata-driven system.

---

## 11. Risks & Mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| Expression DSL becomes a Turing-tarpit | High | Keep it minimal & declarative; Rune escape hatch for real scripting |
| Dynamic data query performance | High | JSONB GIN indexes early; promote hot fields to columns (hybrid); benchmark in Phase 2 |
| Metadata schema churn breaks stored models | High | Strict additive versioning; migration jobs; golden export tests |
| Studio UI scope explosion | High | Ship Phase 8 in slices; allow JSON import as the "power user" fallback |
| All-Rust frontend ecosystem gaps | Medium | Keep Runtime UI logic-focused; fall back to React for Studio if needed |
| Security holes in dynamic API | High | RLS + layered authz + audit + pen-test; never trust client-supplied metadata |
| Multi-tenant data leak | Critical | Tenant_id in every index + RLS + automated tenant-isolation tests |
| Scope creep ("build Salesforce") | Critical | Ruthless MVP per phase; defer everything not in the roadmap |

---

## 12. Immediate Next Steps (Week 1)

1. **Confirm decisions in §5** (especially §5.1 storage and §8 frontend) — these are hard to reverse.
2. Scaffold the Cargo workspace (`crates/` layout in §6).
3. Stand up dev environment: `docker-compose.yml` with Postgres 16 + Redis + the server binary.
4. Implement Phase 0: config, tracing, health, migration runner, empty `meta` schema.
5. Write the **first E2E test scaffold** so vertical testing is ready for Phase 1.
6. Create `docs/adr/` (Architecture Decision Records) — record the §5 decisions formally.

---

## 13. Open Questions for You

1. **Deployment target:** self-hosted on-prem, cloud (AWS/GCP/Azure), or both? Affects multi-tenancy & storage choices.
2. **Team size & Rust experience:** drives how aggressive the timeline and the all-Rust-frontend choice are.
3. **Must-have integrations on day one** (e.g., specific ERP, email provider, SSO) — these shape Phase 9.
4. **Licensing/commercial model:** open core? proprietary? Affects dependency choices.
5. **Is there a reference system you admire** (Salesforce, ServiceNow, Odoo, FileMaker, Retool, Appsmith)? Naming it sharpens the design language.
6. **Expression language appetite:** happy with a restricted declarative DSL, or do you expect users to write real scripts?

---

*This plan is a living document. Update §5 decisions into `docs/adr/` as they are confirmed, and revise the roadmap in §9 as estimates firm up.*
