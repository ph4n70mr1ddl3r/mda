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
5. **Multi-tenant ready** — tenant isolation from day one (`tenant_id` + PostgreSQL Row-Level Security, §5.4).
6. **Auditable** — Every record change, workflow transition, and security decision is logged.
7. **API-first** — Everything the UI can do, the API can do.
8. **Domain-neutral** — the engine knows no business nouns (no "invoice," "ledger," "period," or "account"). CRM, ITSM, ERP, or any vertical is an *app built on top* as metadata + sandboxed extensions — never a feature baked into the core. The test for any proposed core feature: *could a different domain use it too?* If not, it belongs in a custom field type / expression function / `wasmtime` module (§5.6) or a reference-app bundle, not in `mda-*`. Demanding domains (ERP being the hardest stress test) are made possible by a strong *generic extension surface*, not by domain-specific code.

### Non-goals (for v1)
- Code generation / compile-step deployment (we interpret, not generate)
- Mobile-native apps (responsive web first)
- Real-time collaborative editing of metadata
- **General-purpose stateless integration broker / iPaaS** (MuleSoft/Boomi class) — we integrate as a **hub** with our own canonical model and business logic, not as a pass-through broker (§5.22)

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
│               audit/event/outbox logs                        │ │
│  (blob bytes live outside Postgres; see 5.14 BlobStore)      │ │
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
2. **Business data model** — the dynamic instances of those entities. Stored as **one real table per entity** (`biz.<table>`) with a stable core schema + hoisted relational columns + native FK constraints + a JSONB payload for the rest (§5.1).

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
| Async jobs | **`apalis`** (Postgres-backed) | Cron, retries, delayed, DLQ; drives scheduled reports, workflow timers, two-phase purge, integration flows (§5.22), notification digests (§5.18), sharing reshare jobs (ADR-0013), and the outbox-drain worker (§5.9.4) |
| Search | PostgreSQL FTS → OpenSearch later | Start simple |
| Frontend | **Leptos** (CSR/SSR, WASM) *or* React + TypeScript | See §8 |
| Frontend build | Trunk (WASM) / Vite | |
| Observability | `tracing` + OpenTelemetry → Grafana/Loki/Tempo | |
| Config | `config-rs` / figment | Env + files |
| Containerization | Docker + docker-compose | Dev + prod |
| CI/CD | GitHub Actions | |

### Alternative considered: SeaORM
SeaORM offers nicer ergonomics but obscures the dynamic SQL we need for runtime data access. **SQLx with hand-written queries for metadata + a query-builder module for dynamic data** is the recommendation.

### Rate limiting
Enforced at the API edge via Tower middleware — **per-tenant and per-user** quotas, returning `429 Too Many Requests` + `Retry-After`. It composes with the per-query **cost budgets** for list and report queries (§5.16/§5.17): a request that would blow the budget is rejected (or routed async) rather than throttled blindly, so expensive metadata-driven queries can't be used to DoS the cluster.

---

## 4. The Meta-Meta-Model (the heart of the system)

This is the schema that *describes the descriptions*. Everything below is a table in the `meta` schema. **All `md_*`, `sec_*`, `sys_*`, and `biz.*` tables are tenant-scoped** — each carries `tenant_id`, is covered by RLS (§5.4), and is cached keyed by `(tenant_id, …)` (§5.3); a tenant's model (entities, fields, forms, …) is its own, never global. (The sketches below omit `tenant_id` for brevity.)

> **Note — fixed meta-model (ADR-0008):** the platform's own definitions (`md_entity`, `md_field`, …) are **static Rust structs + SQL**, not first-class runtime entities. This is deliberate (avoids the bootstrapping / infinite-regress of self-hosting meta-metadata); the consequence is "no edit the editor." See §5.12.

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
  ├─ md_report_dataset   - STRUCTURED query (§5.17): base_entity + fields[ref traversals]
  │                       + filters/having (DSL AST) + group_by + params + limit
  ├─ md_report_grouping
  └─ md_report_chart     - bar/line/pie/table params
md_report_schedule - cron + recipients + format (pdf/xlsx/csv) + running_user (§5.17)
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
sec_role_hierarchy                           -- role tree (optional; "see records below me"); hierarchy-derived shares are epoch-gated by a per-tenant hierarchy_epoch (ADR-0013)
sec_share(record, principal, access)         -- explicit/manual record share
sec_share_rule(entity, cond, principal, access, epoch)  -- criteria-based auto-share (ABAC); epoch = rule version for revoke-safe invalidation (ADR-0013)
sec_record_share(record_id, principal_id, access, rule_id, epoch)  -- MATERIALIZED computed shares; revoke-safe via epoch invalidation (ADR-0013). Per-record recompute is synchronous in the write txn; admin rule/hierarchy changes bump an epoch (instant revoke) + async batched recompute (progressive grant).
-- (ABAC is expressed through the six grains above — record predicates + field constraints — via the
--  expression engine, §5.11; there is no separate policy table.)
```

### 4.6 Integration
```
int_connector    - typed adapter; CORE ships universal transports (HTTP/DB/file/MQ/GraphQL/SOAP)
  └─ int_endpoint        + a pluggable Format + Auth boundary (§5.6/§5.22) so niche formats & vendor
                          protocols (EDI, IDoc, AS2/OFTP, vendor auth) are EXTENSION connectors
int_mapping      - field mapping between external & internal entities (per-flow transform steps)
int_flow         - inbound/outbound ETL pipelines — HUB model: external ↔ internal canonical entities
  └─ int_flow_step       each step may apply an expression-engine transform (value map/conditional/debatch)
int_value_map    - code-set translation tables (e.g. status codes) used by mapping steps — data, not code
int_external_id  - correlation registry: (entity, record_id, system, external_key) — idempotent, dedupe,
                   bidirectional sync between a platform record and an external system's natural key (§5.22)
int_schedule     - cron / event triggers for flows
int_webhook      - outbound event subscriptions (contract: §5.21)
```

### 4.7 System
```
sys_audit_log    - every write, who/when/what (before/after JSONB)
sys_outbox       - transactional outbox: durable pending side-effects (webhook/email/ETL/event),
                   inserted in the same txn as the data write; drained by workers (at-least-once). §5.9
sys_event_log    - canonical sequence-numbered domain-event stream: real-time push + replay only (§5.10);
                   compliance audit is the separate, heavier sys_audit_log (before/after JSONB)
sys_notification - user-facing notifications: (id, tenant_id, user_id, type, entity, record_id,
                   payload JSONB, read_at, created_at) — created by rules/workflows/system events,
                   pushed over the user:<id>:notifications channel (§5.10); read/unread via read_at
sys_lock         - soft advisory record checkout (owner, ttl, heartbeat) for UX coordination. §5.9
sys_setting      - key/value config
sys_translation  - i18n strings (metadata/UI only; data-level i18n deferred — U5)
sys_version      - metadata versioning / migration tracking
sys_impex_job    - bulk import/export job: (type, entity, format, mode, mapping JSONB,
                   status, stats JSONB, source/result blob refs, created_by, timestamps) §5.13
sys_impex_row    - per-row import result (resumability + error report): (job_id, row_no, status, action, record_id, errors)
sys_blob         - attachment storage metadata: (id, storage, storage_key, filename, mime, size, checksum,
                   owner_id, scan_status, created_at) — bytes live in the BlobStore, not here. §5.14
sys_blob_ref     - back-references for orphan cleanup / dedup: (blob_id, entity, record_id, field)
sys_secret       - secret REFERENCE only (the value lives in the SecretStore, never here): (id, tenant_id, name, kind, ref, rotated_at, created_at) §5.20
sys_notification_preference - per-user notification opt-in/out by (type, channel): (user_id, type, channel, enabled) §5.18
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
| **Core (every table)** | `id` ULID PK (stored as native `uuid`, 16 bytes — preserves ULID's b-tree-friendly monotonic sort order; never `text`/37 bytes, which would bloat every index and FK), `tenant_id`, `owner_id`, `state`, `version BIGINT`, `created_at`, `updated_at` | Fixed across all entities; `tenant_id` in every composite index + RLS policy; `version` drives optimistic concurrency (§5.9); **no `deleted_at`** (deletion = hard-delete + archive, §5.7 / ADR-0006 / ADR-0015). All timestamps are `timestamptz` **stored as UTC** (see *Timestamps & time zones* below) |
| **Hoisted relational** | one real typed column per reference field, e.g. `ref_customer_id ULID` | Real `FOREIGN KEY ... ON DELETE <behavior>`; `DEFERRABLE INITIALLY DEFERRED` for mutual references |
| **Hoisted scalar (optional)** | a **generated** column for indexed/unique/hot fields, e.g. `email TEXT GENERATED ALWAYS AS (attributes->>'email') STORED` (+ `UNIQUE`/index on it) | **JSONB `attributes` is the single source of truth** for scalars; the generated column is a *derived*, queryable/indexable/unique-able projection that is **never written directly** — so there is no dual-write inconsistency. (Contrast reference fields below, where the real column *is* the source and the value is absent from JSONB.) |
| **Flexible payload** | `attributes JSONB` | Everything else; GIN-indexed for ad-hoc query |

**Rules:**
- A reference field is **always** hoisted to a real column with a real FK — never stored only in JSONB. Non-negotiable; this is the whole point of choosing Pattern B.
- Plain scalars default to JSONB; promote to a **generated column** (derived from `attributes`) when they need an index, a unique constraint, or heavy filter load. JSONB remains the source of truth; the generated column is never written directly. (A genuine real-column source-of-truth is reserved for reference fields, above.)
- Indexes (incl. unique) on JSONB values use expression or generated-column indexes.
- Table DDL is generated and applied **at publish time** by the DDL/migration engine, never ad hoc at runtime.

**Timestamps & time zones.** Every timestamp column (core `created_at`/`updated_at`, `sys_*` logs, archive, outbox) is `timestamptz` **stored as UTC**; the DB never stores local time. A per-tenant **timezone** (IANA name) is a *display/parsing* concern only: the Runtime UI formats UTC into the user's zone, and inputs are parsed in that zone to UTC on write. Time-based scheduling — scheduled-report cron (`md_report_schedule`, §4.4) and workflow timers (§5.9.6) — is expressed in the configured/tenant timezone and **resolved to UTC at fire time**, so SLA escalations and report schedules behave correctly across geographies.

**Why not pure JSONB (Pattern D)?** A value inside JSONB cannot carry a real FK, so RI would have to be enforced in the application layer — re-implementing existence checks, cascade rules, orphan sweeps, and race handling that Postgres already provides correctly. See `docs/ri-strategies.md` §3 for what that costs. Not worth it without Salesforce's justification.

### 5.2 Expression language (DSL)
Business rules, validations, workflow guards, report filters all need expressions ("`amount > 1000 AND status == 'open'`").
- **Option 1:** Embed a scripting language (`rhai` / Rune / Boa) — powerful, slower, sandbox risk.
- **Option 2:** Build a small typed expression AST evaluator in Rust (like `eval` crates or custom).
- **Recommendation:** **Custom JSON AST evaluator** stored as JSONB, evaluated by a Rust interpreter. Safe, serializable, fast, testable. Add a `rune` escape hatch later for power users behind a capability flag.
- **Safety (bounded evaluation — REVIEW.md U6):** every evaluation is capped — max AST depth, max node count, max function-call budget, and a step counter; exceeding any returns a bounded error (never a panic/hang). Pure functions by default; any I/O-capable function is individually allowlisted and timeout-bounded. Together with the rule-recursion budget (§5.9.5) this prevents a pathological or malicious expression from DoSing the system.

### 5.3 Hot reload of metadata
Metadata changes must take effect without restart:
- Cache metadata in a read-through in-memory cache (`moka`), **keyed by `(tenant_id, entity_id)`** — never by entity id alone, or the cache is a cross-tenant leak (§5.4). All metadata cache keys are tenant-scoped to match the RLS isolation boundary.
- Invalidate via PostgreSQL `LISTEN/NOTIFY` on a `meta_changed` channel (or Redis pub/sub). **`LISTEN/NOTIFY` is the fast path, not the only path** — it is lossy across reconnects/replicas, so each instance also runs a low-frequency **version-stamp poll** (compare the cached model version to `md_active_version`) as a self-healing fallback (REVIEW.md §5.3).
- Every metadata read carries a version stamp so the runtime detects staleness and re-reads on mismatch.

### 5.4 Multi-tenancy
- **Strategy A:** `tenant_id` column on every table + app-enforced filter. Simple, shared-everything.
- **Strategy B:** Schema-per-tenant in PostgreSQL. Strong isolation, more ops.
- **Recommendation v1:** **Strategy A** with PostgreSQL **Row-Level Security** enforcing tenant isolation at the DB layer (defense in depth). Make tenant_id part of all composite indexes.
- **Tenant lifecycle (deferred, alongside U9):** provisioning a new tenant (creating the tenant row, seeding the first admin, and bootstrapping its `biz.*` tables on first publish) and suspending / deleting a tenant (draining its `biz` + `biz_archive` tables and blobs per §5.14/§5.15) are **not yet specified** — tracked here as a later-phase ops concern, not designed in v1.

### 5.5 Versioning & migrations of metadata
Fully specified in **§5.8** (draft → validate → publish → activate lifecycle) and **§4.8** (lifecycle tables). In short: metadata is deployable as JSON bundles (Studio export/import) across dev → staging → prod; publish classifies changes as additive / transforming / two-phase destructive and runs a validated migration against live data.

### 5.6 Extensibility model
- **Field types:** registry of built-in types + a Rust trait `FieldType`; built-ins compile into the binary, and **custom / tenant-specific field types, expression functions, and rule actions run as sandboxed `wasmtime` modules** — a first-class extensibility boundary that lets customers ship logic safely without forking the platform (REVIEW.md §5.6).
- **Functions:** the expression DSL calls registered Rust functions (e.g., `now()`, `sum()`, custom); custom functions are also deliverable as `wasmtime` modules.
- **Connectors:** trait `Connector` is a **pluggable Format + Auth boundary**. The core ships only *universal* transports/formats (HTTP/DB/file/MQ/GraphQL/SOAP) and standard auth handlers (bearer, basic, mTLS, OAuth client-credentials, signed requests). Niche formats (EDI X12/EDIFACT, IDoc, AS2/OFTP) and vendor-specific protocols/auth (e.g. a vendor's CSRF-token dance) are **extension connectors** — adapters or `wasmtime` modules — never built into the engine, and never encoded as domain document types. The platform parses a format's *envelope*; what a document *means* is a mapping the integrator authors (§5.22).
- **Webhooks:** outbound HTTP on domain events.
- **Where domain behavior lives (domain neutrality, principle 8):** the core ships only *generic* types and functions. Domain-specific behavior — money rounding rules, FX conversion, tax calculation, depreciation, posting logic, document-numbering schemes beyond the generic `auto_number` — is implemented as **custom field types / expression functions / `wasmtime` modules here**, never as new core types. Consequently an ERP, CRM, or vertical is a *reference-app bundle* (metadata + extensions, transportable via the §5.8 export/import) built on this surface, not a change to the engine. Hard rule: if only one domain needs it, it doesn't belong in `mda-*`.

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
  reference_qualifier     -- optional expression restricting valid targets (hard-enforced at the referencing record's write; also filters the Studio lookup picker)
  rollup_summary          -- optional: aggregate children onto parent
                          --   (count/sum/avg/min/max of a field)
                          --   computed incrementally & synchronously in the
                          --   child's txn by default; async opt-out for hot
                          --   parents (ADR-0017)
                          --   NB: distinct from a *calculated field* (same-record
                          --   formula via the DSL, §5.2) — a rollup aggregates
                          --   *other* (child) records.
```

**Many-to-many** is materialized as a real **join table** `biz.<a>_<b>` with a composite PK and FKs to both sides — real constraints, real integrity. The join-table name is derived deterministically from the two entity table names in **ascending lexicographic order** (`biz.<smaller>_<larger>`), with a collision check against existing entity table names at publish (§5.8); a name that would collide requires an explicit override name on the relationship.

**Mutual references** (A references B, B references A) use `DEFERRABLE INITIALLY DEFERRED` FKs so integrity is checked at commit, not per statement.

**Calculated fields (same-record formulas).** Distinct from a rollup (which aggregates *other* records, above and ADR-0017): a calculated field is a DSL formula (§5.2) over fields of the *same* record (e.g. `line_total = qty * unit_price`). It is **stored (hoisted), not virtual** — recomputed **synchronously in the write transaction** whenever one of its declared dependencies changes (same side-effect slot as after-rules, §5.9.3 step 5), so it is transactionally consistent with the write and queryable/indexable like any scalar. It is **write-protected**: clients never set it directly (the engine overwrites it from the formula).

**Deletion (recorded decision — ADR-0006):** deletion is **hard-delete + archive**. The row moves to `biz_archive.<table>` (carrying `archived_at` / `archived_by`); native `ON DELETE CASCADE` / `SET NULL` / `RESTRICT` fire naturally, preserving RI end to end, and the archive gives recoverability/undo. **Soft-delete is rejected**: it defeats native cascade (the row isn't actually deleted, so `ON DELETE …` never fires) and would force RI to be re-implemented in the app layer (the ServiceNow/Salesforce route).

Consequences:
- The core columns carry **no `deleted_at`** (§5.1) — `biz` tables hold live rows only.
- This also resolves REVIEW.md **U3**: unique constraints need no `WHERE deleted_at IS NULL` partial-index gymnastics — a deleted value (e.g. email) can be re-created immediately, since the old row is gone from the live table.
- **Restore (ADR-0015)** is an explicit, **batch-scoped** operation, not a flag flip. Archive is trigger-driven: a `BEFORE DELETE` trigger on each `biz.<table>` copies the row to `biz_archive.<table>` with an `archive_batch_id`, and because every cascaded table has its own trigger, native `ON DELETE CASCADE` archives the **whole cascade tree** under one batch id. Restore re-inserts all rows of a batch in dependency order (parents before children) in one transaction, re-running FK/cascade checks; the restored row gets a **new, higher `version`** so any stale client hits a clean 409 (OCC, §5.9), and `SET NULL` refs are **not** auto-relinked (restore undeletes rows, not every side effect).

### 5.8 Metadata lifecycle: draft → validate → publish → activate

**Problem being solved (REVIEW.md C2, enables C3):** metadata describes data that already exists and that other metadata depends on. You cannot freely mutate live metadata — it must be validated, may require DDL + data migration, must be previewable before activation, and must be rollbackable. Therefore **all edits go through drafts; publish is the only path to activation.** There is no "edit the active model directly."

**Three states:**
- **Active** — the live, published model. The `md_*` tables *are* the active model; runtime reads them directly (fast, simple hot path).
- **Draft** — an editable, in-progress model stored as a JSONB document (`md_draft`). Not visible to runtime.
- **Snapshot** — an immutable archive of a prior active model (`md_snapshot`), for history, diffing, and rollback.

**Lifecycle:**
1. **Branch** — create a draft from the current active model (or from a snapshot).
2. **Edit** — Studio mutates the draft JSONB. v1: one editor per draft (checkout lock + optimistic `version_etag`); multi-editor collaboration is a v2 extension. Multiple drafts per tenant are allowed, but there is **no merge**: if two drafts diverge, each publish is re-validated against the *then-current* active model and applies sequentially (last publish wins on conflicting changes). Collaborative/3-way merge is a v2 concern.
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
- **Formula dependency DAG check** — calculated fields (§5.7) and rollup summaries (ADR-0017) form a directed graph (a calculated field may reference other calculated fields; a rollup references child fields). Detect cycles at validate time (a cyclic set, e.g. `C = A + B` and `B = C + 1`, is a hard publish error), enforce a max dependency depth, and confirm every referenced field exists and is type-compatible. This is the publish-time analog of the runtime recursion budget (§5.9.5) — without it a cyclic formula set would only blow up at runtime.
- Reserved-name / duplicate-name collisions
- Row-count estimate for transforming ops (warn on large tables)

**Publish execution — staged migration + atomic cutover (ADR-0011).** Atomicity and resumability cannot both live in one transaction: a single txn is all-or-nothing (nothing to resume), while resumability implies multiple txns (not atomic). The resolution is to split publish into a resumable staging phase that is *invisible to the runtime*, and a short atomic cutover that flips the model — using **expand/contract** so the expensive data movement happens before the flip:

- **Phase A — Staging (resumable, not visible):** the draft enters `publishing` (one per tenant, enforced by partial unique index — this replaces a long-held advisory lock as the gate). A background job (apalis) runs the *large/transforming* ops only: `ADD COLUMN _v2_<name>`, batched backfill, `CHECK … NOT VALID` + `VALIDATE CONSTRAINT` for make-required, `CREATE INDEX CONCURRENTLY` for new indexes (CONCURRENTLY cannot run in a txn, so index builds are *always* staged). Each batch is its own transaction, checkpointed to `md_migration_log` → **resumable on failure, abortable with no user impact** (old model still served). A final delta backfill at cutover (or a temporary sync trigger for very-hot tables) closes the backfill race.
- **Phase B — Cutover (atomic, short):** `pg_advisory_xact_lock` acquired *inside* a transaction kept to a **single-digit-second budget** — final delta backfill → contract (rename old → `_<name>_old`, rename `_v2_` into place, attach staged indexes `USING INDEX`, `SET NOT NULL` now metadata-only) → additive DDL → destructive *retire* (metadata only; purge stays deferred) → apply `md_*` diff → archive prior model to `md_snapshot` → bump `md_active_version` → commit → broadcast `meta_changed`. Failure rolls the txn back; `md_active_version` is unchanged; staged artifacts remain for retry or cleanup.
- **Post-publish cleanup:** renamed `_<name>_old` columns are dropped after a short grace (default 24h) by a scheduled job — a bad cutover is recoverable within the grace window before resorting to snapshot rollback.

> **Rule of thumb (ADR-0011):** the cutover transaction must complete in single-digit seconds. Any op that can't is staged in Phase A and reduced to metadata-only at cutover. This is what makes the model genuinely atomic (old-or-new, never half) while the expensive work is resumable.
>
> **Rollback caveat:** a reversing transform is not guaranteed lossless (e.g. `numeric → integer` truncates; rollback restores the *truncated* values). The migration plan flags lossy transforms at validate time so reduced rollback fidelity is explicit.

**Two-phase destructive deletes:**
- **Retire** — set `status = retired` on the `md_*` definition (`md_entity` / `md_field` / `md_relationship`) and record a pending-purge row in `md_retirement`. Data preserved; runtime & UI hide it; queries exclude it. Fully reversible (un-retire clears the status and removes the `md_retirement` row).
- **Purge** — scheduled job after grace. Before the irreversible drop, data is **exported to cold storage** (S3/Parquet) — a column's values, or the whole `biz.<table>` + its `biz_archive` for a dropped entity — keyed for compliance retention (ADR-0015, §5.15). Then the column/table is dropped. **Irreversible to the live schema** (the metadata definition is retired); a cold-storage export is for compliance reads, not one-click restore (undoing a purge = re-create via publish §5.8 + bulk-import §5.13). Blocked if any non-retired dependency still references it.

**Rollback:** keep last N snapshots (default 10). Rollback loads the snapshot as a draft and re-publishes (reverse migration). **Caveat:** rollback cannot restore data already purged by a two-phase destructive op; retire-phase changes are fully reversible.

### 5.9 Concurrency & transactional semantics

(Resolves REVIEW.md **C4**.) The system must (a) prevent lost updates, (b) keep multi-step writes atomic, and (c) guarantee that external side-effects are delivered exactly without coupling request latency to external systems. The answers below are deliberately standard patterns — do not invent novel concurrency.

**1. Record-level concurrency: optimistic by default.**
- Every `biz.<table>` carries a `version BIGINT` (core column, §5.1) — this is the **per-row OCC counter**, distinct from the model's `md_active_version` (§4.8) and a draft's `version_etag` (§5.8). Updates are conditional:
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
5. **After-rules, synchronous** — data side-effects (set fields, update related rows, incl. parent rollup deltas — ADR-0017) within the same txn; failure rolls back the whole write
6. **Recompute this record's `sec_record_share` rows synchronously** — only if owner or a sharing-rule-relevant field changed; O(rules) for one record (ADR-0013). Keeps record-level security fresh with zero per-record revoke lag. **Caveat — rollup × sharing (ADR-0017):** a synchronous rollup delta that lands on a *parent's* sharing-rule-relevant field must also trigger **that parent's** share recompute here; without it, a sharing rule keyed on a rollup (e.g. "share Account where `total_invoices` > 10k") would go stale until the parent is next written. **Owner transfer** (reassignment) is itself a permissioned, audited write — gated as an action (§5.11 grain 5) or a dedicated `transfer_ownership` capability — and triggers this same recompute.
7. **Write the three logs** (all in the same txn): `sys_audit_log` (full before/after JSONB — the **compliance** record, §4.7), `sys_event_log` (lightweight domain events for **real-time/replay**, §5.10), and `sys_outbox` rows (**async side-effects**: email, webhook, ETL kick — §5.9.4). Audit and event are **distinct tables** — different schema and retention (§5.15) — not one.
8. `UPDATE … version + 1` (OCC)
9. Commit
10. *(Workers drain `sys_outbox` — see §5.9.4)*

**4. Transactional Outbox pattern (the key decision).** External/integration side-effects (webhook, email, pub/sub, scheduled kicks) are **never** called inside the data transaction. Instead:
- The data change and an `sys_outbox` row are inserted in the **same transaction** → the dual-write problem is eliminated: *if the data committed, the side-effect is durably queued.*
- A worker claims rows with `SELECT … FOR UPDATE SKIP LOCKED`, performs the external call, retries with exponential backoff + jitter, and routes persistent failures to a **dead-letter** set for manual replay.
- Delivery is **at-least-once**; therefore all consumers must be **idempotent** (stable message id; webhooks carry an idempotency header; internal processors dedupe via the outbox row's own idempotency key + `status=processed` — the processed outbox rows *are* the dedupe log, retained past the re-delivery window).

This yields a clean, answerable rule for "are rules/workflows trustworthy or eventual?":
- **Data-affecting logic = trustworthy** (synchronous, in-transaction, atomic with the write).
- **Notification/integration side-effects = eventual** (durable via outbox, at-least-once).

**5. Rule & workflow execution model.**
- Rules fire **synchronously within the write transaction** for data effects; async-only side-effects go to the outbox.
- Multiple rules matching one event are ordered **deterministically: `priority` then `id`**.
- **Recursion budget:** a synchronous side-effect that re-triggers after-rules is capped (default depth 10); exceeding it aborts the transaction. Guards against rule loops (ties to expression-engine limits, REVIEW.md U6).
- A workflow transition is a specialized update: guards evaluated → `state` set → on-transition actions run in-transaction → notifications to outbox. A transition that triggers *another* transition chooses **sync or async** chaining (ADR-0016):
  - **Sync (default for in-process chains)** — the next transition runs **in the same transaction**, atomic and all-or-nothing, bounded by the recursion budget above (extended to transitions) and the lock-ordering/deadlock-retry rules of §5.9.7. Use for immediate dependent state progressions (Approve → auto-Fulfill where Fulfill is pure data). A cycle aborts at the depth cap.
  - **Async (for cross-system / long-running / external)** — emit a domain event to the outbox; the receiver runs later, timer-style, with `FOR UPDATE` + state re-check (§5.9.6), idempotently via that re-check (at-least-once, §5.9.4). These are **eventual and may partially complete**, so the modeler MUST declare failure handling — a `failure_state` (a designated workflow state, not new schema), retry policy, and optional compensation; on exhaustion the record moves to `failure_state` and alerts fire. The engine rejects an unhandled async chain at publish (§5.8). **No silent partial completion.**
  - Either way, every transition attempt + outcome lands in `sys_event_log` (§5.10), so partial completions are always visible and auditable.

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

**2. Event source: `sys_event_log`.** The canonical, sequence-numbered, transactional domain-event stream (written in the same txn as the data change, §5.9.3 step 7). **Real-time and replay read from here**; the heavier **compliance audit** (full before/after JSONB) is a *separate* table, `sys_audit_log` (§4.7) — different schema, different retention (§5.15: event 7–30 d for replay, audit 1–7 yr for compliance). Reconciling the three write-path logs: `sys_event_log` = the lightweight **facts** (what happened, for push/replay); `sys_audit_log` = the heavy **compliance record** (before/after, for audit queries); `sys_outbox` = the **work items** (async delivery needing a worker — webhook/email/ETL). The relay reads `sys_event_log`; audit queries read `sys_audit_log`; workers drain `sys_outbox`.

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
- `user:<id>:notifications` — personal notifications (persisted in `sys_notification`, §4.7; read/unread state)
- `tenant:<id>:broadcast` — system-wide (incl. `metadata.published` → UI reloads model, tying into cache invalidation §5.3)

**4. Relay & fan-out.** Each app instance runs a relay holding its locally-connected SSE clients. On a new `sys_event_log` row:
- A trigger fires `NOTIFY mda_event` (payload = seq); each instance's background `LISTEN` reads the row and fans out to local clients by channel.
- Postgres NOTIFY fans out to all LISTENing instances across the cluster, so **no Redis is required for the core DB→app hop**.
- Scale path: if NOTIFY volume becomes a bottleneck, front it with Redis Pub/Sub (or a stream) as the cross-instance bus; the SSE clients and `sys_event_log` contract stay unchanged.

**5. Reliability: `Last-Event-ID` replay.** SSE clients send `Last-Event-ID` (= last `seq` seen) on (re)connect. The server replays `sys_event_log WHERE seq > $last AND matches-subscription`, then switches to live. Result: **at-least-once delivery to the client within the retention window** — no missed events across reconnects. **Beyond the window** (a client offline longer than the `sys_event_log` retention, §5.15) replay is impossible by design — the client must do a **hard full re-sync** (refetch the active model and current record state on next page load). For the `metadata.published → UI reloads model` broadcast (the `tenant:<id>:broadcast` channel, §5.10.3) this full-refetch is the defined fallback rather than expecting a replayed event.

**6. AuthZ on the channel (critical).** A client must only receive events for records/fields it is authorized to see:
- Authenticate the SSE connection (JWT).
- Authorize each subscription (can this user see this entity/record/view?).
- The relay filters events per client using the same RBAC+ABAC+data filters as the REST API (§5.11) — including **field-level visibility** (never leak a change to a masked field). Access decisions are cached to keep this cheap.

**7. Client merge strategy (ties to OCC §5.9).** On receiving `record.updated` for the record the client is viewing:
- Not editing → refresh the view.
- Editing (unsaved changes) and `to_version` advanced → show a conflict banner ("changed by someone else — Review / Overwrite / Refresh") *before* the user wastes effort. The 409-on-save remains the backstop. This is the UX payoff of combining OCC + real-time.

**8. Presence (lightweight).** "Who else is viewing/editing this?" — clients heartbeat `POST /api/presence/:entity/:id` (~15s); the server tracks viewers in Redis (TTL) keyed by (record, user) and broadcasts **presence deltas** (who joined/left the view). This is distinct from the explicit edit-level **soft checkout** (`sys_lock`, §5.9.2): presence = passive "who's looking" (no correctness effect); `sys_lock` = active "I'm editing" claim (advisory banner) whose acquire/release emit the `record.checked_out`/`record.released` events (§5.10.2). Presence does not emit those events.

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

**Performance — materialized sharing, revoke-safe (ADR-0013).** Computing "can U see R?" live is expensive (ownership + team + shares + rules + hierarchy), so criteria-rules and hierarchy are denormalized into **`sec_record_share(record_id, principal_id, access, rule_id, epoch)`** and list queries join against it. The governing principle: **it is always safe to under-grant temporarily, never safe to over-grant** — so the table may lag on grants but must be instantly correct on revokes.

- **Revoke by invalidation, not recomputation.** Each sharing rule (and the role hierarchy) carries a monotonic **`epoch`**; each share row records the epoch it was computed under. Enforcement honors a share **only if `share.epoch ≥ rule.current_epoch`**. Bumping a rule's epoch is a single O(1) update that instantly invalidates all stale shares — **there is no window in which a revoked share is honored.** A GC job later removes superseded rows.
- **Split recompute by trigger.** A **record write** recomputes *that record's* shares **synchronously, in the write transaction** (O(rules) for one record — bounded), so a record's own shares are always fresh and per-record revocation has zero lag. An **admin change** (rule edit, hierarchy re-parent, OWD change) bumps the epoch (instant revoke) and enqueues a **batched, resumable** recompute job (apalis) — grant-side catch-up is progressive, revoke-side correctness already guaranteed.
- **Additive grants don't invalidate.** Adding a rule/member/manual-share is purely additive (can never revoke), so it never bumps an epoch — only adds new shares. Editing an existing rule's condition is treated as revoke-conservative (bump + recompute), since "strictly broader" is undecidable in general.
- **Thundering herd is bounded.** Bulk recomputes are batched, parallelized, idempotent, resumable, rate-limited per tenant, with Studio-visible progress/ETA. A failed reshare leaves partial-but-valid (under-grant) state — never over-grant.

Ownership / team / manual-share layers are evaluated **live** from the cached effective context (cheap, no materialization); only criteria-rules and hierarchy are materialized and epoch-gated. Epoch bumps broadcast via `meta_changed` (§5.3/§5.10) to invalidate effective-context caches on all instances promptly (correctness holds regardless, but the broadcast keeps revoke latency low).

**4. Field level (FLS).** `sec_field_permission(role, entity, field, access ∈ {none, read, write})`:
- `none` → hidden: dropped from read responses, rejected on write.
- `read` → returned but read-only; rejected on write.
- `write` → fully editable.
- Enforced in the **serialization layer** (read projection) and the **deserialization/mutate layer** (write rejection).

**5. Action / transition level.** `sec_action_permission(role, action_id)` gates workflow transitions and custom actions — checked at the action invocation boundary. Modeling transitions as explicit actions (not just "update") is what lets you say "only the Approve role can run the Approve transition."

**6. Value constraints (write ABAC).** `sec_field_constraint(entity, field, condition, message)` — per-role conditions evaluated by the expression engine at write time (e.g. "role=sales_rep ⇒ discount ≤ 0.05"). **Distinct from validations** (`md_rule` / field validations): validations are universal data-correctness rules; field constraints are authorization-scoped ("*who* may set *what*"). Both use the same engine. **Composition (ADR-0012):** when a user holds several roles that grant write on a field, the applicable constraints *intersect* — all must hold — so a constraint cannot be bypassed by adding a permissive role. A role granting only `read` imposes no write constraint.

**Effective-context caching.** At session start, compute the user's effective context — roles (direct + via teams/groups), teams, role-hierarchy ancestors, and compiled sharing-rule predicates — and cache it (Redis, TTL) keyed by `user_id`; invalidate on role/team/hierarchy/sharing-rule change. **Invalidation is a tenant-scoped broadcast** over the `meta_changed`/event channel (§5.3/§5.10), not a local call — a role/team change processed on instance A must invalidate sessions on instance B too. Correctness holds regardless (the next check re-reads from the `sec_*` tables), but the broadcast keeps authorization changes prompt across the cluster, and is load-bearing for sharing-rule/hierarchy *epoch* invalidation (ADR-0013). Every grain consults this cached context rather than re-querying the `sec_*` tables per request.

**Token revocation (AuthN).** Invalidating the *cache* is not enough on its own: a stateless JWT remains valid until it expires. So **access tokens are short-lived** (minutes), and a gateway-checked **revocation list** (or a `token_epoch` claim on the user, bumped on logout / role-narrowing / suspension / termination) is evaluated on every request, so a revoked or compromised session is killed **before natural expiry**. Refresh tokens are rotated and independently revocable. This closes the gap between "authorization changed" and "the old token stops working."

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
| Report dataset | Report engine (compile time) | Structured model compiled by the engine — FLS projection + per-entity record predicate by construction (§5.17) |
| GraphQL | Service layer (shared with REST) | Per-field FLS + record predicate at every relationship level; depth/cost limits (§7, ADR-0010) |

**No negative permissions.** The model is purely additive on top of a deny-by-default baseline (matches Salesforce; avoids deny-vs-grant precedence ambiguity) — *for binary capabilities* (object/field/action grants: union across assigned roles). Value constraints (grain 6) are **not** negative permissions: they do not revoke a write capability, they predicate it. Predicates compose by **intersection** (ADR-0012), which is the standard composition of conditional grants and the only rule that prevents a permissive role from overriding a restrictive one. For an exception on capability, narrow the role or add a more specific OWD/sharing rule.

### 5.12 Meta-model: fixed, not self-hosting

(Decides REVIEW.md **U8**, ADR-0008.) Salesforce makes its own model first-class (`EntityDefinition` is itself an entity the Studio can edit). We do **not**, for v1: the meta-model (`md_entity`, `md_field`, `md_relationship`, …) is fixed Rust structs + SQL, edited by dedicated Studio handlers — not treated as runtime entities.

**Why:** self-hosting introduces bootstrapping and an infinite-regress of meta-meta-(meta-)tables, adds large complexity for modest v1 gain, and every reference platform that isn't Salesforce (Odoo, Dataverse, ServiceNow's core) ships a fixed core model. Pragmatism wins.

**Consequence:** "no edit the editor" — the Studio cannot redefine the platform's own definition tables. Custom entity/field types remain extensible via the registry + `wasmtime` (§5.6). Revisit only if a use case demands user-defined meta-types.

### 5.13 Bulk data import/export (record level)

(Resolves REVIEW.md **U1**.) A day-one enterprise need: move many records in/out via CSV/XLSX/JSON, validated and safe. The engine **reuses the runtime write pipeline** — an import is just batched, mapped writes, so an imported row is indistinguishable from one typed by hand (no second set of rules to drift).

**Import flow:**
1. **Upload** the source file → stored as a blob (§5.14); create a `sys_impex_job` (status=pending).
2. **Map** source columns → entity fields (auto-match by name; manual override). For reference fields, choose the **lookup field** on the target entity (e.g. resolve Customer by `name`). Specify the **key field** for update/upsert matching (must be unique).
3. **Validate (dry-run):** parse + validate every row against field types, required, uniqueness, **FK existence** (resolved via the lookup), and **authz** (object / field / record — can the user create/update this row and write these fields, §5.11). Produce a report: N to create, N to update, N errors (row + field + message). **Nothing is written.**
4. **Commit** — two policies: *all-or-nothing* (one transaction; any error aborts) or *best-effort* (batched transactions, e.g. 500 rows; valid rows commit, errors reported per row in `sys_impex_row`).
5. **Result:** downloadable error report (CSV) + summary stats; successful rows are audited like any write (§4.7) and appear immediately (real-time, §5.10).

**Modes:** `create` | `update` | `upsert` (match by key field). Owner/team default to the importing user (or a mapped column).

**Scale & reliability:** large files run as an **apalis job** (ADR-0007), streaming + batched, with progress and **resumable** per-row results. Idempotent on job id; re-running a failed import resumes from the last committed batch.

**Export:** from any list/view (current filter) or ad-hoc query → CSV/XLSX/JSON, field-selectable, streaming; respects **field-level read** security (can't export fields you can't read); reference fields export display value or id (configurable). Large exports run as a job with a download link.

### 5.14 Attachments & blob storage

(Resolves REVIEW.md **U2**.) Records carry file attachments (a signed PDF on a Customer). Bytes never live in Postgres; a storage abstraction holds them and metadata is tracked in `sys_blob`.

**Field type:** `attachment` — value is a single `blob_id`, or a JSONB array for multi-file. Registered like any field type (§5.6); the `biz` column holds the id(s), the bytes are external.

**Storage abstraction:**

```
trait BlobStore: Send + Sync {
    async fn put(&self, key, stream, meta) -> Result<()>;
    async fn get(&self, key) -> Result<ByteStream>;
    async fn signed_upload_url(&self, key, ttl) -> Result<Url>;   // S3 presign
    async fn signed_download_url(&self, key, ttl) -> Result<Url>;
    async fn delete(&self, key) -> Result<()>;
}
```

Impls: `LocalBlobStore` (dev/small), `S3BlobStore` (S3 / MinIO / GCS-via-S3). Configurable per deployment.

**Upload:** client requests a **presigned upload URL** (or proxies via the API for local) → uploads bytes directly to the store → the API records `sys_blob` (mime, size, **sha256 checksum**, owner) → the attachment field stores the blob id. Multipart/resumable for large files.

**Download:** the API issues a **short-TTL signed URL** (or streams bytes with an authz check). The signed URL is bound to the user/record so a leaked URL can't bypass authz.

**Security:** an attachment inherits its record's access — read/download only if the user can read the record **and** the field (field-level security, §5.11); upload requires write on the field.

**Integrity & lifecycle:**
- **Dedup** by checksum (two references to one file share a single blob).
- **Virus scan** hook (async, e.g. ClamAV): block download until `scan_status=clean`; quarantine infected.
- **Thumbnails/previews** generated async for images/PDFs.
- **Cleanup:** when an attachment field is cleared or the record is hard-deleted (ADR-0006), `sys_blob_ref` back-references drive an orphan-cleanup job that deletes blobs with zero refs. MIME allowlist + per-file/per-record size limits enforced on upload.

### 5.15 Retention of high-volume append-only tables

(Resolves REVIEW.md **U4**.) `sys_audit_log`, `sys_event_log`, and `sys_outbox` are append-only and grow unbounded; without a strategy the DB balloons and queries slow.

- **Time partitioning** (Postgres declarative partitioning, weekly/monthly) so old data is dropped via cheap `DROP TABLE` / `DETACH` instead of slow `DELETE`.
- **Retention per table** (configurable per tenant): `sys_audit_log` → 1–7 yr (compliance); `sys_event_log` → 7–30 days (only needed for real-time replay, §5.10); `sys_outbox` → purge on successful delivery (keep DLQ entries longer).
- **Archival:** expired audit partitions export to cold storage (S3 / Parquet) before drop, satisfying retention beyond the hot DB.
- **Two archive tiers beyond `sys_*` (ADR-0015):** `biz_archive.<table>` (operational undo of record hard-deletes) carries a per-tenant **undo-TTL** (default 30–90 days); **cold-storage purge exports** (taken before a destructive metadata purge drops a column/table, §5.8) follow audit retention (1–7 yr). Both are managed by the same partition/lifecycle job.
- **Sampling** (optional, per entity): full audit for sensitive entities, sampled for very-high-volume ones.
- A partition-management **apalis** job (ADR-0007) pre-creates future partitions and drops/archives expired ones on schedule.

### 5.16 Threat model: untrusted metadata

(Addresses REVIEW.md **§11** correction.) In this system, *metadata is user-authored logic* — Studio users write rules, expressions, field definitions, report queries, and workflow guards that the engine executes at runtime. That is a unique attack surface vs. a normal app, and it is treated explicitly.

- **Governing principle:** the runtime only ever acts on metadata it **loaded from the DB after authz** — never on logic a client sends in a request payload. Clients send *data*; the server decides *logic* by reading trusted, published metadata (ties to the draft→publish gate, §5.8).
- **Resource exhaustion** — a hostile expression/query → bounded evaluation (§5.2) + rule recursion budget (§5.9.5); list queries are paged and cost-limited; report datasets carry a cost budget, result cap, timeout, and join-depth limit (§5.17).
- **Data exfiltration** — expressions/queries run under the caller's AuthZ (object/field/record, §5.11) in *every* context (rule, report, workflow, API). No path bypasses field-level visibility: report queries are a structured model compiled by the engine, which projects only visible fields and injects record-security at every joined entity (§5.17).
- **Privilege escalation via metadata** — editing/publishing metadata requires an elevated Studio role; runtime reads cannot mutate the model.
- **Injection** — all SQL is parameterized; the query builder emits bound parameters, never string-interpolated values; the DSL cannot emit raw SQL.
- **Cross-tenant leakage** — Postgres RLS (§5.4) + `tenant_id` in every index + automated isolation tests (§11).
- **Sandboxed extensions** — `wasmtime` modules (§5.6) are capability-scoped and resource-limited; they reach the DB/network/filesystem only through explicitly granted, audited host functions.

### 5.17 Reporting query model & security

(ADR-0014 — resolves the report-query gap: the other place, besides expressions, where Studio users author logic the engine executes.) Left as "query + params + joins," `md_report_dataset` is an open surface: cartesian-product DoS, field/record leakage, and an unspecified author-vs-runner AuthZ question. The resolution: a report query is a **structured metadata model compiled by the engine**, never raw SQL — and because the engine builds the SQL, it enforces field- and record-level security **by construction** and bounds cost.

**1. The query model is metadata, not SQL.** `md_report_dataset` is a structured declaration the engine compiles to parameterized SQL over the `biz` tables:

```
md_report_dataset
  base_entity          -- root entity (FROM)
  fields[]             -- (traversal, field, alias, aggregate?) — traversal hops references from base
  filters              -- expression AST (the §5.2 DSL), parameterized — never raw SQL
  group_by[]           -- grouping fields (each a traversal + field)
  having               -- expression AST over aggregates
  order_by[]           -- (field, dir)
  parameters[]         -- named params bound at run time (:region, :as_of)
  limit / offset       -- pagination / result cap
```

Reference traversals (e.g. `customer.region.name`) resolve to **real joins over hoisted FK columns** (§5.1/§5.7) — indexed, no string keys. `filters`/`having` use the **bounded expression DSL** (§5.2), so the whole query inherits the DSL's safety (no raw-SQL emission, bounded evaluation). **There is no raw-SQL report path in v1**; power users who need more get the structured model + DSL, with a `wasmtime` extension (§5.6) or a deferred raw-SQL-behind-a-capability-flag feature as the escape hatch.

**2. AuthZ = the runner's, enforced by construction.** The author's permissions gate only *who may edit the report* (a Studio permission); at run time only the runner's effective context applies. Because the engine compiles the SQL, security is structural, not bolted on:
- **Object** — runner needs `read` on every entity in the traversal, else the report errors for them.
- **Field, projection** — a `select` field the runner lacks FLS `read` on is **dropped** (column omitted); the report degrades shape, never leaks.
- **Field, semantic positions** — a field the runner can't read that appears in `filter`/`group_by`/`having`/`order_by` is a **run-time error** for that runner, *not* a silent drop: dropping a filter/group/order changes semantics (a dropped filter could reveal rows). The Studio previews this ("runner X cannot run this report: groups by `salary`").
- **Record** — the runner's record-security predicate (OWD + shares + rules, §5.11 / ADR-0013) is injected as a WHERE — and critically **at every entity in the traversal, not only the base**: `FROM Invoice JOIN Customer` filters both the Invoices *and* the Customers the runner may see, so a join cannot leak a Customer the runner cannot read. Inner vs left join is the author's choice; the per-entity predicate applies regardless.
- **Aggregates + FLS** — an aggregate over a field the runner can't read is as sensitive as the field; that column is dropped/errored per the rules above.

**3. Cost control / DoS.** Reports can be expensive; they are cost-limited like list queries but more so:
- **Cost estimate + budget** — before execution the engine estimates scan/join cardinality against a per-tenant budget; over budget → refuse, or (if the report is marked large-ok) run **async as an apalis job** with a download link (same pattern as bulk export, §5.13).
- **Result cap + timeout** — synchronous runs are capped on rows and wall time; overruns are killed.
- **Non-hoisted JSONB access** — a field still in `attributes JSONB` (not hoisted, §5.1) forces a seq scan; the estimator flags it. Remedy is to hoist at publish (§5.8) or mark the report async-only. Report authors don't control hoisting, so this guard is what keeps a naive report from full-scanning.
- **Join-depth limit** — traversals are depth-capped (as GraphQL is, ADR-0010) to deny exponential nested joins.

**4. Scheduled reports run under a configured running user.** `md_report_schedule` carries a **running user** (default: the schedule's owner). The job executes the dataset under *that* user's AuthZ; recipients receive that fixed, filtered output — not a per-recipient re-filtering (matches Salesforce). Per-recipient views require per-recipient schedules. Because the context is captured at run time, revoking the running user's access (ADR-0013) correctly stops the schedule from leaking.

**5. Dashboards & exports reuse the same path.** Dashboard widgets (§4.2) run datasets under the *viewer's* context (interactive). Exporting a report's result set reuses the bulk-export field-read rules (§5.13) — you cannot export fields you can't read.

### 5.18 Notifications & messaging

A first-class platform subsystem, not just a table. Built on `sys_notification` (§4.7) + the real-time channel (§5.10) + the transactional outbox (§5.9.4). The engine knows no notification *content* — types and templates are metadata; this is the generic delivery machinery.
- **Notification types are metadata** (authored in Studio): a key (e.g. `invoice.overdue` — the *name* is the modeler's; the engine treats it opaquely), source event (rule / workflow / system), default channels, and a link to a template (§5.19).
- **Per-user preferences** (`sys_notification_preference`, §4.7): a user may mute a type or opt out of a channel; defaults come from the type definition and are overridable per user. **Honored at fan-out time** — a muted type is never produced, not just hidden.
- **Multi-channel delivery:** in-app (write `sys_notification` + push over `user:<id>:notifications`, §5.10.3), plus email / SMS / push. Every channel *except in-app* is an async side-effect routed through the outbox (at-least-once, idempotent, §5.9.4). Channel implementations are pluggable (a `Channel` trait, analogous to `Connector`, §5.6).
- **Digest / batching:** a type may be marked digestible; a scheduled job (apalis) rolls undelivered notifications for a user within a window into one message (prevents notification storms from a bulk event).
- **AuthZ:** a notification inherits its record's access — a user is notified about, and can open, only records they can read; the push payload is field-level filtered exactly like the SSE relay (§5.10.6). Retention follows §5.15.

### 5.19 Templating

A template engine for rendered output — email bodies, notification text, and document/mail-merge (invoice PDF, contract, letter). Templates are metadata authored in Studio; the *content* is domain data, the *mechanism* is generic.
- **Template store** `md_template(name, kind[email|document|message], body, content_type, locale?)`: the body is a **sandboxed template DSL** — a restricted subset of the expression engine (§5.2) plus variable interpolation. It is **bounded-evaluated** (§5.2 limits), has **no arbitrary code / no I/O**, and cannot emit raw SQL. Variables are the render context (record fields, actor, params).
- **Render context is AuthZ-filtered:** a template renders under the *recipient's* (or running user's) field-level visibility (§5.11) — a template can never emit a field the recipient cannot read, same structural rule as reports (§5.17).
- **Document / mail-merge:** render a record (or a list) through a document template to PDF/DOCX/HTML, reusing the report renderer path (`mda-reports`, §6). Triggered by a rule/action/workflow and delivered async via the outbox if external.
- **Localization:** a template carries a locale; the resolver picks the best match for the recipient/tenant from `sys_translation`. (Template *labels* are translatable; record *data* i18n remains deferred — U5.)

### 5.20 Secrets management

Connectors (§4.6), integrations, and outbound channels (§5.18) need credentials — API keys, DB passwords, OAuth tokens — per tenant, never in plaintext metadata.
- **Reference vs. value** (same pattern as blobs, §5.14): `sys_secret` (§4.7) holds only a *reference* (`name`, `kind`, `ref`, `rotated_at`); the **secret value lives in an external `SecretStore`**, never in Postgres.
- **`trait SecretStore`** with impls: `LocalSecretStore` (dev — encrypted file / OS keyring) and cloud KMS (AWS KMS / GCP Secret Manager / Azure Key Vault / HashiCorp Vault). Configurable per deployment.
- **Resolution:** secret values are resolved **server-side only**, at the moment a connector/channel runs, under that connector's authz. Values are **never** returned by any API, **never logged** (`tracing` redacts known-sensitive fields), and **never serialized** into events, audit, or outbox payloads.
- **Rotation & audit:** `rotated_at` is tracked; rotation is an explicit operation; every resolution is audited (who/when resolved which secret). References are tenant-scoped (RLS, §5.4).

### 5.21 Event & webhook contract

The canonical **outbound** event contract for integrations. `sys_event_log` (§5.10) is the *internal* stream; this is the *external* contract delivered to webhook subscribers (`int_webhook`, §4.6). The contract is structural; event *types* and payloads are metadata/extension-defined.
- **Envelope:** every delivery is a versioned JSON envelope `{event_id, tenant_id, schema_version, type, entity, record_id, occurred_at, actor, data}`. `event_id` is the idempotency key (§5.9.4); `schema_version` lets consumers evolve without breakage.
- **Signing (integrity):** each delivery is **HMAC-signed** (e.g. `X-MDA-Signature: t=…,v1=…`) with a per-subscription secret held in the SecretStore (§5.20); recipients verify origin and the timestamp guards against replay within a window.
- **Delivery:** via the outbox (at-least-once) with backoff + jitter + DLQ (§5.9.4); recipient acks with 2xx, otherwise retried.
- **Subscription model:** a webhook subscribes to `(event types, entity filter, optional ABAC filter)`; the relay applies AuthZ so a subscriber receives events only for records/fields its principal may see (same per-client filtering as the SSE relay, §5.10.6) — a webhook can never exfiltrate fields its principal lacks.
- **Replay:** a subscriber may request replay from a bookmark (`event_id`) within the retention window (§5.15), mirroring SSE `Last-Event-ID` (§5.10.5).

### 5.22 Integration architecture

Integration is a *capability* of the application platform: the platform syncs and orchestrates with external systems **because it is a participant with its own canonical model and business logic** — not a wire-level relay. Becoming a general-purpose stateless broker / iPaaS is an explicit **non-goal** (§1); that is a different product. Everything below is generic data-integration mechanics — it introduces no vendor or business noun (principle 8).

**1. Topology: hub, not broker.** Every flow materializes data into the platform's canonical `biz.*` entities (§5.1); there is **no stateless A→B pass-through**. This is deliberate: it lets the hub apply AuthZ, audit, rules, workflows, and transformation *between* systems — a strength a bare broker lacks. Consequence: high-volume stateless brokering is out of scope; batch/eventual sync and orchestration are in. Two vendor systems (e.g. an ERP and an SCM) are integrated by each syncing to/from the platform's canonical model, with the platform's rules engine doing the mediation.

**2. Flows, steps, mapping.** An `int_flow` is an inbound (external→internal) or outbound (internal→external) pipeline of `int_flow_step`s, scheduled by `int_schedule` or triggered by an event (outbox/webhook, §5.9.4/§5.21). `int_mapping` binds external fields to internal fields; **each step may run an expression-engine transform** (§5.2) — value translation (`int_value_map`, §4.6), conditional mapping, aggregation, enrichment (lookup from another entity), and **debatching** (one inbound message → N records) / batching (N records → one message). Transforms reuse the bounded DSL, inheriting its safety (§5.2/§5.16) — there is no second scripting surface to drift.

**3. Correlation & idempotency — the external-ID registry.** Reliable bidirectional sync requires linking a platform record to an external system's natural key. `int_external_id(entity, record_id, system, external_key)` (§4.6) is that registry: it drives **upsert by external key**, **idempotent re-delivery** (a flow keyed on `external_key` won't double-apply), and **dedup** when the same business event arrives via two paths. Without it, sync silently duplicates or drops — this is the single most important reliability primitive for multi-system integration.

**4. Conflict resolution (system of record).** When two sources update the same logical record, the platform applies a **declared policy**, never a guess. Each flow/mapping carries a conflict policy: `last_write_wins` (by comparable timestamp), `source_priority` (a named system wins per field-group), `field_level_sor` (per-field system-of-record), or `manual` (quarantine for a human). This is distinct from internal OCC (§5.9): OCC prevents lost updates *within* the platform; the conflict policy reconciles *across* systems. A failed reconciliation is a flow error (→ DLQ, §5.9.4), never silent corruption.

**5. Reliability & delivery.** Inbound/outbound delivery is **at-least-once via the transactional outbox** (§5.9.4): retries with backoff + jitter, DLQ, idempotent consumers keyed on the external-ID registry. Flows are resumable (checkpointed, apalis) and observable (per-flow run history — tracked in §14). Secrets for outbound auth live in the SecretStore (§5.20).

**6. The Connector boundary stays pluggable (domain-neutral).** Per §5.6: the core ships only universal transports/formats and a pluggable Format + Auth handler. EDI, IDoc, AS2/OFTP, and vendor protocols/auth are **extension connectors** — the platform parses a format's *envelope*; what a specific document *means* is a mapping (§5.22.2) authored on top. The core never names a vendor and never encodes a business document type.

---

## 6. Component Breakdown (crate / module level)

Use a **Cargo workspace** to keep boundaries clean.

```
mda/
├── Cargo.toml                 # workspace
├── crates/
│   ├── mda-core/              # shared types, errors, Result, ids, traits
│   ├── mda-meta/              # metadata model structs + loader + cache
│   ├── mda-data/              # dynamic data access (query builder, CRUD over biz tables: hoisted cols + JSONB)
│   ├── mda-expression/        # DSL AST + evaluator + functions registry
│   ├── mda-security/          # authN, authZ (RBAC+ABAC), data filters
│   ├── mda-workflow/          # state machine engine + timers
│   ├── mda-rules/             # business rule engine (triggers/conditions/actions)
│   ├── mda-reports/           # report dataset builder + renderers (pdf/xlsx/csv)
│   ├── mda-integration/       # connectors, mappings, ETL flows
│   ├── mda-audit/             # write-path logging: sys_audit_log + sys_event_log + sys_outbox rows (all written together in the write txn, §5.9.3 step 7); the outbox-DRAIN worker itself runs in mda-server via apalis
│   ├── mda-api/               # HTTP handlers (Axum) — the "edge"
│   └── mda-server/            # binary: wires everything, config, bootstrap
├── migrations/                # SQLx migrations for meta schema
├── web/                       # frontend (Leptos or React)
├── docker/
├── docs/
└── tests/                     # end-to-end + integration
```

> **Crate granularity vs. team size (§13 Q2).** This 12-crate split is the *target* decomposition for a real team; for a solo or small team it is heavy boundary overhead. Start with a smaller core set (e.g. `mda-core`, `mda-meta`, `mda-data`, and `mda-api`/`mda-server`) and split a crate out on demand as a module accrues distinct responsibilities. The boundaries above are logical contracts, not a mandate to pre-create every crate up front.

### Module responsibilities (detail)

- **mda-meta** — Load `md_*` tables into typed structs; expose `MetadataCache` (query by entity id, invalidate on change). Defines the canonical `Entity`, `Field`, `Form`, `View`, etc. types used everywhere.
- **mda-data** — Given an `Entity` and an operation (create/read/update/delete/list), produce the correct SQL against `biz.<table>` (real tables with hoisted relational/scalar columns + a JSONB `attributes` payload — ADR-0001). Query builder for list views (filters/sort/paging over hoisted columns + JSONB). Handles validation, defaults, computed fields, rollup deltas (ADR-0017).
- **mda-expression** — Parse/evaluate the DSL. Inputs: expression AST (JSON), record context, function registry. Returns typed values. Used by rules, workflows, validations, reports.
- **mda-security** — `Identity` (user/tenant/roles/teams); multi-grained `check` (object / field / record / action, §5.11); ABAC via the expression engine; injects record-level predicates into `mda-data` queries; tenant isolation via Postgres RLS (§5.4).
- **mda-workflow** — State machine: given entity + current state + transition request, evaluate guards (expressions), execute actions, persist new state, enqueue tasks/notifications.
- **mda-rules** — Triggers: before/after CRUD, on-event, on-schedule. Sequence: match → condition → action. Actions: set field, call function, fire event, send webhook, enqueue.
- **mda-reports** — Build dataset (run parameterized query against data layer), apply grouping/aggregation, render to table/chart/pdf/xlsx.
- **mda-integration** — `Connector` trait (pluggable Format + Auth boundary, §5.6); hub-model inbound/outbound flows with expression-engine transform steps, the external-ID registry, and per-flow conflict policy (§5.22); scheduled via apalis; idempotent via external key + at-least-once outbox delivery.
- **mda-api** — **REST** (OpenAPI via `utoipa`) for Studio, auth, and simple CRUD; **GraphQL** (`async-graphql`) as a first-class runtime data API (ADR-0010) for relationship traversal. Routes map to the service layer; same engine, different authz per surface.
- **mda-server** — `main.rs`: load config, init DB pool, warm metadata cache, mount routers, start workers, graceful shutdown.

> **The §5.18–5.22 subsystems are logical modules, crated on demand** (per the granularity note above), not new crates on day one: templating (§5.19) lives alongside the renderers in `mda-reports`; secrets (§5.20) are a `SecretStore` trait in `mda-core` with impls wired in `mda-server`; notification fan-out/digest workers (§5.18) and integration/webhook delivery workers (§5.21/§5.22) run in `mda-server`, with the in-app push path through `mda-api`'s SSE relay (§5.10).

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
DELETE /api/data/:entity/:id                            # hard-delete + archive (ADR-0006)
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

### GraphQL (first-class runtime API, ADR-0010)
For a dynamic, relationship-rich data model, GraphQL is a strong fit — clients traverse references (`customer { invoices { lineItems { } } }`) and fetch exactly what they need. It runs alongside REST (which stays for Studio, auth, and SSE):
```
POST /api/graphql      # schema generated from metadata; authz + field-level security apply per field
```
The schema is derived from the active model and re-generated on publish (§5.8). **Prototype in Phase 2** alongside the dynamic data layer; enforce query depth/cost limits to deny expensive nested queries. Like reports (§5.17), GraphQL enforces **record-security at every relationship level** of a nested query and applies FLS per field — it shares REST's service layer, so the same predicates apply; there is no GraphQL-specific bypass.

> **MVP scope (ADR-0010).** "First-class" means a supported, security-enforced runtime surface — not that every verb ships on day one. At MVP GraphQL is **query/traversal-first** (reads + nested fetches over relationships); **mutations** (create/update/delete, workflow actions) reach REST parity progressively in later phases. Until then, clients mutate via the REST data API (`/api/data/:entity/*`) and read/traverse via GraphQL — which is also why §5.10.1 routes SSE-client mutations through REST. Both surfaces share one service layer, so mutations land on GraphQL with no new authz path when added.

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

> **Decision deferred to a Phase 0 spike (ADR-0009):** the drag-and-drop Studio designers are the highest-risk, highest-effort component. Phase 0 builds a throwaway metadata-driven form renderer in *both* Leptos and React to make the call on evidence rather than committing now. Until then the frontend stack is provisional.

---

## 9. Development Roadmap (Phased)

Each phase is independently valuable and demoable. Aim for vertical slices.

> **MVP milestone (the de-risk target).** The credible MVP is one vertical slice: *define an entity via the Studio API → publish → CRUD via the runtime API → a **basic** rendered form + list UI → login/auth.* Its server-side prerequisites are Phases 1–3 (metadata engine, dynamic data layer, auth); on top of those, a **minimal** form+list UI is built (dashboards, fancy views, and real-time are skipped for the MVP). It lands around **week 26** — aligned with the re-estimate below, when a basic runtime UI exists — **not at week 55**. Everything heavier — full Studio designers, workflows, reporting, integrations, real-time, bulk import, attachments — is **post-MVP**. Ship the MVP to real users early to validate the model-driven core before committing to the long tail.

### Phase 0 — Foundation (Weeks 1–3)
- Cargo workspace skeleton, CI, Docker, dev Postgres+Redis
- `mda-core`: error types, IDs (`ulid`), traits
- Config, logging (`tracing`), health endpoint
- SQLx setup + migration runner; `meta` schema skeleton
- **Frontend spike:** throwaway metadata-driven form renderer built in *both* Leptos and React to pick the Studio tech on evidence (§8, ADR-0009) before Phase 6/8 — runs in parallel with infra setup
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
- Field type registry: string, text, integer, decimal(p,s), money (decimal + ISO currency), bool, date, datetime (timestamptz), enum, reference, json, **auto_number** (gapless, concurrency-safe sequence — generic, every app needs auditable IDs) — precision/scale are metadata-driven and participate in publish-time transforms (ADR-0011); richer domain types (FX money, tax amounts) are custom types via §5.6, not core
- Reference fields → real typed columns with native `FOREIGN KEY` (per §5.7)
- Validations (**declarative only**: type, required, defaults) — DSL/rule-based validations arrive in Phase 4
- **Optimistic concurrency (§5.9):** `version` column + `If-Match`/ETag on PATCH → 409 on conflict
- `/api/data/:entity/*` runtime routes
- **GraphQL prototype (ADR-0010):** schema generated from the active model alongside the dynamic data layer; both REST and GraphQL sit on the same service layer; enforce query depth/cost limits to deny expensive nested queries. (Prototype scope in Phase 2 per ADR-0010; REST-parity mutations are added progressively in later phases — not MVP-blocking.)
- **Deliverable:** Create/read/update Customer records via REST and a prototype GraphQL endpoint, fully driven by metadata, with real FK-enforced relationships; adding/renaming/dropping a field runs a validated migration.

### Phase 3 — Security & Auth (Weeks 9–11)
- Users, teams, roles (`sec_*`); JWT auth + refresh tokens
- **Object-level RBAC** (`sec_permission`) on every route
- **Field-level security** (`sec_field_permission`: none/read/write) — enforced in serialization + write
- **Record-level**: ownership + team baseline (`sec_owd`), app-layer predicate injection in `mda-data`; tenant isolation via Postgres RLS (§5.4)
- Effective-context caching (roles/teams) per session
- Audit logging (`sys_audit_log`) on all writes
- *Deferred to Phase 6:* criteria-based sharing rules, role hierarchy, materialized `sec_record_share` (§5.11 / ADR-0013) — full record-level security for the runtime UI's list views
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

### Phase 6 — Form & View Definitions + Runtime UI (Weeks 17–26)
- `md_form`, `md_view`, `md_dashboard`, `md_navigation`
- `/api/forms/:entity`, `/api/views/:entity` return renderable JSON
- **Build the Runtime UI** (Leptos): dynamic form renderer, list/grid renderer, dashboard, navigation shell
- **Real-time channel (§5.10):** SSE over `sys_event_log` with `Last-Event-ID` replay; conflict banner when a viewed record is changed by another user
- **Advanced record security (deferred from Phase 3):** criteria-based sharing rules, role hierarchy, and materialized `sec_record_share` with epoch invalidation (§5.11 / ADR-0013) — full record-level security behind list/detail views
- **Deliverable:** A user logs in, sees a menu, opens a list of Customers, creates/edits via a rendered form, and is alerted in real time when another user edits the same record — all from metadata, zero hardcoded pages.

### Phase 7 — Reporting (Weeks 26–29)
- `md_report`, datasets, grouping, charts
- Renderers: HTML table, XLSX, PDF, CSV
- Scheduled reports via job queue + email delivery
- Dashboards consume report datasets
- **Deliverable:** Build a "Sales by Month" report in Studio, run it, export to PDF, schedule daily email.

### Phase 8 — Studio UI (Weeks 29–43)
- Entity/field designer, form designer (drag-drop), view designer, report designer, workflow designer, rule editor, security admin
- Metadata import/export/promote UI
- This is large; consider parallelizing across designers
- **Deliverable:** A business analyst can build a small CRM app entirely through the browser.

### Phase 9 — Integration Layer (Weeks 43–46)
- **Hub-model integration (§5.22):** `Connector` trait with universal transports (HTTP/DB/file/MQ/GraphQL/SOAP) + a pluggable **Format + Auth** boundary; niche formats / vendor protocols are *extension* connectors (§5.6), not core
- `int_*` metadata: flows/steps, field mapping with **expression-engine transforms** (value maps, conditionals, debatching), scheduling (cron + event triggers)
- **External-ID registry** (`int_external_id`) for upsert-by-external-key, idempotent re-delivery, and cross-path dedup
- **Conflict policy** per flow/mapping (`last_write_wins` / `source_priority` / `field_level_sor` / `manual`) — cross-system reconciliation, distinct from internal OCC
- Outbound webhooks via the §5.21 contract (signed, versioned, replayable) + inbound webhook receiver; connector secrets in the SecretStore (§5.20)
- Reliable delivery via the transactional outbox (§5.9.4); flows run as resumable apalis jobs
- **Deliverable:** Bidirectionally sync a platform entity with an external REST API on a schedule, keyed by external ID, with a declared conflict policy — no duplicates, no silent drops.

### Phase 10 — Bulk Data Import/Export & Attachments (Weeks 46–49)
- **Bulk import/export (§5.13):** CSV/XLSX/JSON; field mapping; create/update/upsert by key; **dry-run** with validation report; all-or-nothing or best-effort; batched transactions for large files; runs as an `apalis` job with progress + resumable per-row results
- **Security:** reuses the full write pipeline (object + field + record authz, §5.11) — can't import into fields/rows you can't write; export respects field-level read
- **Attachments (§5.14):** `attachment` field type; `BlobStore` trait (local + S3); `sys_blob` metadata; presigned upload/download URLs; async virus-scan hook; checksum dedup; thumbnails; cleanup on record delete (ADR-0006)
- **Deliverable:** Upload a CSV of Customers → map fields → dry-run → commit (with an error report); attach a PDF to a Customer and download it via a short-TTL signed URL.

### Phase 11 — Hardening, Scale, Polish (Weeks 49–55)
- Observability (tracing/OpenTelemetry dashboards), load testing
- Metadata cache tuning, query optimization, materialized views for reports
- i18n (`sys_translation`), theming
- Backup/restore runbook, blue-green deploys
- Security review, pen-test, OWASP hardening
- Documentation, SDK generation, sample apps
- **Deliverable:** Production-ready v1.0.

> Timelines are rough planning anchors for a small team; **Phases 6 and 8 were re-estimated honestly** (REVIEW.md Timeline Reality Check): Phase 6 ≈ 9 weeks for a from-scratch metadata-driven UI renderer + real-time; Phase 8 ≈ 14 weeks for the full drag-and-drop Studio (parallelizable only with a real team). The full sequence is now ~55 weeks; a solo developer should ~double it — which is why the **MVP milestone above exists** (ship value around week 26, not week 55).

> **Deferred (explicitly, not omitted — closes REVIEW.md U5 / U9).** Deliberately out of v1 scope:
> - **U5 — data-level i18n** (translatable enum/reference data; record-level multi-language fields). `sys_translation` covers **metadata/UI strings only** for v1; data i18n is a later, opt-in feature once a real multi-locale tenant needs it.
> - **U9 — high availability & replication** (Postgres HA, read replicas for reporting, logical replication / warm standby, connection-pool tuning). Single-node Postgres for v1; HA is a Phase 11 hardening activity when production scale demands it.
> - **Search** — §3 lists "PostgreSQL FTS → OpenSearch later" but it is not yet designed: which fields are searchable, how results respect record/field security and tenancy, and the FTS→OpenSearch migration path. Scoped into a later phase once a concrete search need arrives; until then, list filters cover structured queries.
> All are tracked here so they remain *visible*, not lost.

> **Further platform capabilities — scoped (§5.18–5.21) or tracked, not yet scheduled to a phase.** Surfaced by a platform-capability review; all are *generic* (domain-neutral, principle 8):
> - **Notifications & messaging (§5.18), templating (§5.19), secrets (§5.20), event/webhook contract (§5.21), integration architecture (§5.22)** — scoped as design sections now; implementation lands alongside the phases that need them (notifications with Phase 5/6, templating with Phase 7, secrets + webhook contract + integration with Phase 9).
> - **Extension connectors (EDI / IDoc / vendor protocols & auth)** — niche formats and vendor protocols are **not** core features; they are extension connectors via the pluggable `Connector` boundary (§5.6/§5.22.6). Build the ones you need as adapters/`wasmtime` modules; the core stays vendor-neutral.
> - **SSO / SAML / SCIM** — enterprise identity is larger than the OAuth2/OIDC in §3: SAML SSO, SCIM user provisioning/deprovisioning, and directory sync. Currently only an open question (§13 Q3); **elevate to a scoped phase** before enterprise adoption — it gates most enterprise sales.
> - **Mass actions** — bulk update / delete / assign / transfer *by filter* (distinct from file import/export, §5.13); interacts with sharing recompute (ADR-0013) and cascade (ADR-0006), so it needs its own design rather than a retrofit. **✅ closed (ADR-0021):** `POST /api/data/:entity/{mass-update,mass-delete}` reuse the single-record write pipeline per affected record (RBAC + FLS + rules + OCC + audit + events), resolve targets under the **write** predicate, support a `dry_run` + hard cap. Assign/transfer are a `set` of owner/team fields.
> - **API versioning** — REST + GraphQL versioning/deprecation strategy for generated SDK clients (§7): breaking-change management via path/header versioning, sunset headers, and parallel schema generations across publishes. **✅ closed (ADR-0022):** a versioning middleware negotiates the major (`X-API-Version` / `Accept: application/vnd.mda+json; version=N`), stamps `MDA-API-Version` on every response, emits RFC-8594 `Deprecation`/`Sunset`/`Link` for deprecated majors, and 400s with `mda.unsupported_version` below the floor — all env-driven so a v2 cutover is operational. Parallel-major schema generation slots in behind the same boundary when a v2 diverges.
> - **i18n (`sys_translation`)** — metadata/UI string translation (Phase 11 hardening). **✅ closed (ADR-0023):** `meta.md_translation` (best-match locale: exact → prefix → default) + `POST/GET/DELETE /api/translations[/:locale]` + `GET /api/i18n/:locale`, injected into the §5.19 template render context (`{{ i18n.ns.key }}`), and included in the tenant-config export/import. U5 record-data i18n remains deferred.

---

## 10. Testing Strategy

| Layer | Tooling | Focus |
|---|---|---|
| Unit | `cargo test` | Expression evaluator, query builder, state machine |
| Integration | `cargo test` + testcontainers (real Postgres/Redis) | Metadata CRUD, dynamic data, rules, workflows |
| E2E | `cargo test` HTTP + (optional) Playwright for UI | Full vertical: define model → use it |
| Property | `proptest` + **golden corpus** | Expression-eval + query-builder invariants; a curated corpus of golden expressions/queries (REVIEW.md §10) |
| Fuzz | `cargo-fuzz` | **Expression evaluator + dynamic query builder** — the two highest-blast-radius components (a bug affects every entity) |
| Load | `k6`/`oha`/`wrk`/`vegeta` | API throughput, metadata cache |
| Metadata regression | Snapshot golden tests | Model export format stability |

**Golden rule:** every Phase deliverable is backed by an E2E test that defines metadata via API and exercises the runtime against it. This is the only way to trust a metadata-driven system.

---

## 11. Risks & Mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| Expression DSL becomes a Turing-tarpit | High | Keep it minimal & declarative; Rune escape hatch for real scripting |
| Dynamic data query performance | High | JSONB GIN indexes early; promote hot fields to columns (hybrid); benchmark in Phase 2 |
| Audit/event-log growth | Medium | Time-partitioned append-only tables + per-table retention + cold-storage archival (§5.15) |
| Metadata schema churn breaks stored models | High | Strict additive versioning; migration jobs; golden export tests |
| Studio UI scope explosion | High | Ship Phase 8 in slices; allow JSON import as the "power user" fallback |
| All-Rust frontend ecosystem gaps | Medium | Keep Runtime UI logic-focused; fall back to React for Studio if needed |
| Security holes in dynamic API | High | RLS + layered authz + audit + pen-test; never trust client-supplied metadata |
| Untrusted metadata (client-authored logic) | High | Runtime executes only DB-loaded, authz-gated metadata (§5.16); bounded eval (§5.2); parameterized SQL; sandboxed `wasmtime` extensions |
| Multi-tenant data leak | Critical | Tenant_id in every index + RLS + automated tenant-isolation tests |
| Integration sync correctness / data drift | High | Hub-only model (no stateless brokering, §1/§5.22); external-ID registry for upsert/dedup (§5.22.3); declared conflict policy (§5.22.4); idempotent at-least-once delivery via outbox (§5.9.4) |
| Secret leakage (connector credentials) | High | `SecretStore` keeps values out of Postgres, logs, events, and API responses (§5.20); server-side-only resolution under connector authz; rotation + access audit |
| Scope creep ("build Salesforce") | Critical | Ruthless MVP per phase; defer everything not in the roadmap; iPaaS/broker is an explicit non-goal (§1) |

---

## 12. Immediate Next Steps (Week 1)

Architectural decisions are settled (§5, recorded as ADRs 0001–0017); the focus is now execution:

1. **Run the Phase 0 frontend spike** (ADR-0009) — a throwaway metadata-driven form renderer in both Leptos and React, to pick the Studio/Runtime UI stack on evidence.
2. **Scaffold the Cargo workspace** (`crates/` layout in §6).
3. **Stand up the dev environment** — `docker-compose.yml` with Postgres 16 + Redis + the server binary.
4. **Implement Phase 0** — config, `tracing`, `/health`, the SQLx migration runner, and an empty `meta` schema.
5. **Write the first E2E test scaffold** so vertical (define-model → use-it) testing is ready for Phase 1.

---

## 13. Open Questions

The architecture no longer blocks on these, but they shape later phases and ops:

1. **Deployment target** — self-hosted on-prem, cloud (AWS/GCP/Azure), or both? Drives HA/replication scope (U9) and multi-tenancy ops.
2. **Team size & Rust experience** — drives timeline realism (§9 solo-vs-team estimates) and the frontend call coming out of the Phase 0 spike.
3. **Must-have integrations on day one** (specific ERP, email provider, SSO/IdP) — shape Phase 9 (integration layer).
4. **Licensing / commercial model** — open core vs proprietary; affects dependency choices.

> Resolved during planning: storage/RI, metadata lifecycle & publish/migration execution, concurrency & workflow chaining, real-time, authorization (incl. sharing materialization & value-constraint composition), expression language, reporting query model, deletion/restoration lifecycle, and rollup-summary semantics are all decided (§5, ADRs 0001–0017). A platform-capability review added notifications, templating, secrets, the webhook contract (§5.18–5.21), and the integration architecture (§5.22).

---

## 14. Tracked, not yet designed (platform)

Acknowledged platform gaps that are real but lower-priority — visible here so they are not rediscovered mid-build. All are generic (domain-neutral).

> **Progress (ADR-0018):** three of these are now **closed** — record/field
> history + as-of, the modeler/tenant observability console (events / outbox /
> migrations / audit surfaces), and the error-code taxonomy. They are marked
> **✅ closed** below. The remainder are still open.
>
> **Further progress (§5.18–5.22 + ADR-0010):** the scoped platform-capability
> design sections are now **implemented** (see `docs/CAPABILITIES.md`):
> notifications & messaging (§5.18, incl. record-reader recipient resolution +
> FLS-under-recipient rendering + SMTP send), templating (§5.19), secrets
> (§5.20), the outbound webhook contract + inbound verification (§5.21/§14),
> and the hub-model integration architecture (§5.22 / Phase 9, incl.
> `field_level_sor` conflict policy, debatching, per-flow running user, and
> cron-scheduled pulls), plus a first-class GraphQL runtime API (ADR-0010,
> reads **and** mutations). Scheduled-job management is now **closed** (generic
> cron scheduler, ADR-0019). The remaining §14 item — tenant-scoped backup/restore
> + data residency — stays open (it ties to the deferred tenant lifecycle).

- **Modeler / tenant observability console** — a tenant-facing view of job / rule / workflow / integration run history and failures (beyond operator `tracing`/OpenTelemetry). Raw material exists (`md_migration_log`, `sys_event_log`); the *surface* is unbuilt.
  - **✅ closed (ADR-0018):** `/api/observability/{events,outbox,migrations,audit}` surface the run/delivery/audit history. The v1 superuser gate is now broadened: a non-admin principal granted the `observability.read` capability (a `("*", "observability.read")` permission) sees the console, with audit `before`/`after` redacted (field-level projection).
- **Scheduled-job management** for modeler-defined schedules (next-run / last-run / failure state) — scheduled rules and integration schedules exist conceptually but aren't managed as a user surface.
  - **✅ closed (ADR-0019):** `POST/GET/PATCH/DELETE /api/schedules` + `/runs` expose next-run / last-run / last-status / failure state + a per-run history, driven by a multi-instance-safe `FOR UPDATE SKIP LOCKED` worker. Dispatch is by `kind`: `report` runs a saved report under the running user, `integration` pulls an inbound `int.flow` from its connector on cadence (scheduled sync, §5.22), `custom` is an extensibility hook. The outbox console still surfaces delivery failure state (ADR-0018).
- **Inbound webhook verification** — shared-secret / signature / replay protection for the Phase 9 inbound receiver (parallels the outbound contract in §5.21).
- **Record / field history as a surfaced capability** — `sys_audit_log` stores before/after for compliance, but "timeline of this record" and "as-of" queries are not yet framed as a platform API.
  - **✅ closed (ADR-0018):** `GET /api/data/:entity/:id/history` (per-field diffs, FLS-projected) and `GET /api/data/:entity/:id/as-of?version=|at=` reconstruct from audit snapshots, gated like a live read.
- **Error code taxonomy + localized error messages** — the platform emits 409s, validation errors, and publish failures with no coherent, i18n-able error model.
  - **✅ closed (ADR-0018):** every `Error` carries a stable `code()` (`mda.<kind>`); the API envelope exposes `code`/`status`/`message`. `code` is the SDK branch key and the i18n message key. Per-field `details` are now surfaced too: a record write failing several field rules returns one `mda.validation` envelope listing every problem (`field` + per-field `code` like `mda.required`/`mda.invalid_type`/`mda.unknown_field`) instead of failing one-at-a-time.
- **Tenant-scoped backup / restore + data residency** — currently DB-level, single-region; granular tenant export/restore and regional placement tie to the deferred tenant lifecycle (§5.4) and HA (U9).
  - **✅ closed (configuration export **and** import):** `GET /api/tenants/export` produces a portable JSON snapshot of a tenant's configuration — the active model (Studio shape) plus reports, rules, workflows, templates, notification types, schedules, the security graph (roles/permissions/field-permissions/OWD/teams), and integration definitions. `POST /api/tenants/import` restores such a bundle into the caller's tenant: the model stages as a reviewable Studio draft, and every config table is **merged by natural key** (same-name role/connector/report updated in place; bundle ids remapped so FK references — permissions→role, flows→connector, schedules→report/flow — stay valid), idempotent and safe into a tenant already carrying bootstrap config. Superuser-only. Full tenant *data* export/restore + regional placement remain tied to tenant lifecycle (§5.4) and HA (U9).

---

*This plan is v0.4 — decisions are recorded as ADRs 0001–0025 (`docs/adr/`); the roadmap was re-estimated in §9 with an explicit MVP milestone. Successive review passes refined publish/migration execution, authorization, sharing, reporting, deletion/restore, and workflow chaining (ADRs 0011–0016, §5.17), then rollup-summary semantics and canonical write-path consistency — audit/event-log split and the per-record share-recompute step (ADR-0017); a platform-capability review then added notifications, templating, secrets, the webhook contract (§5.18–5.21), the formula-dependency DAG check (§5.8), and the hub-model integration architecture (§5.22). The §5.18–5.22 cluster and GraphQL (ADR-0010) are now implemented (`docs/CAPABILITIES.md`). Team hierarchy (ancestor-team visibility) + a superuser admin security API ship as ADR-0025. Treat it as a living document: amend via new ADRs rather than silently editing settled decisions.*
