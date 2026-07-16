# Referential Integrity in Model-Driven Platforms — How Others Do It

A survey of how production metadata-driven platforms handle referential integrity (RI), distilled into named patterns with a concrete recommendation for MDA. This is the reference material behind REVIEW.md gap **C1**.

---

## 0. The core tension

In a model-driven system, relationships are defined by data (a row in `md_relationship`), not by code. The moment you do that, you have two options for *enforcing* it:

1. **Native DB constraints** (real `FOREIGN KEY`) — strong, fast, declarative, but requires the reference value to live in a **real typed column** on a **real table**. This forces a table-per-entity storage model and a DDL engine.
2. **Application-layer enforcement** — the runtime checks existence, applies cascade rules, and cleans up orphans itself. Works with any storage model (EAV, JSONB, universal tables, soft refs) but is weaker, racy, and you re-implement what the DB already does.

**Every platform picks a point on this line, and the choice is dictated by their storage architecture — usually driven by multitenancy constraints, not preference.**

---

## 1. How each major platform actually does it

### Salesforce (Force.com) — *universal tables + app-layer RI*
- **Storage:** shared multitenant "universal tables." Each object type maps to a wide physical table with a fixed budget of typed columns; overflow fields spill into a key-value (Name/Value) table. Records of *all* tenants' "Account" objects share one physical table, partitioned by `org_id`.
- **References:** stored as ID string values in a column. **No real SQL FK constraints** are possible — the shared-table architecture forbids it.
- **RI enforcement:** entirely in the application/runtime tier, driven by the metadata data dictionary.
- **Key design — two relationship strengths in metadata:**
  - **Master-Detail** — parent *owns* child. Cascade delete is automatic, child cannot exist without parent, ownership/sharing inherited from parent, and **roll-up summary fields** become possible (sum/count of children on the parent). Strong RI.
  - **Lookup** — loose, optional reference. Configurable delete behavior: *Clear the value* (SET NULL), *Don't allow deletion of the referenced record* (RESTRICT), or *Delete this record* (CASCADE).
- **Why this way:** 100k+ tenants on shared infrastructure; per-tenant real tables/DDL is operationally impossible at that scale.

### ServiceNow — *real tables per entity, soft references*
- **Storage:** MySQL. **Each table is a real physical table** generated when the table is created (DDL is issued). Columns are real. (Table extension/inheritance is flattened with copied columns or joined class tables.)
- **References:** stored as a **string/GUID column** holding the target `sys_id`. **Not a real SQL FK constraint.**
- **RI enforcement:** application layer. Each reference field carries a delete behavior (`cascade` / `clear` / `none`) and a "dependent" flag (dependent rows go when parent goes).
- **Reference qualifiers** constrain *which* targets are valid at write time, but aren't hard constraints.
- **Why this way:** real tables give queryability and partial typing, but soft refs avoid DDL pain when relationships change and let the platform own cascade semantics.

### Odoo — *real tables + real native FK constraints*
- **Storage:** PostgreSQL. **Real table per model**, generated at module install, with **real columns per field**.
- **References:** `Many2one` creates a **real `FOREIGN KEY ... ON DELETE {cascade|restrict|set null|set default}`** constraint, chosen via the field's `ondelete=` attribute.
  - `One2many` is *virtual* — not a real constraint, it's the reverse computed view of the owning `Many2one`.
  - `Many2many` creates a **real join table** with a composite PK → real constraints both directions.
- **RI enforcement:** **native Postgres**, fully. The platform just declares `ondelete` and lets the DB enforce.
- **Why this way:** database-per-tenant (historically), no extreme multitenancy pressure, and they accept the DDL/migration cost as the price of strong integrity.

### Microsoft Dataverse (Dynamics 365) — *real tables + native FKs + behavior matrix*
- **Storage:** SQL Server, **real table per entity**.
- **References:** **real SQL FK constraints** with a sophisticated **relationship behavior matrix**:
  - **Parental** — full cascade (delete/share/assign children).
  - **Referential** — clear the reference on parent delete (SET NULL).
  - **Referential, Restrict** — block parent delete while referenced (RESTRICT).
  - **Restrict** — hard block.
  - **Remove Link** — clear reference only.
- **Why this way:** enterprise-grade; chose strongest guarantees, owns the DDL engine.

### OutSystems / Mendix (low-code) — *model → DDL generation → native constraints*
- **Storage:** real tables generated from the visual model at deploy time.
- **References:** **real native FK constraints**. The model is essentially a compile step to schema.
- **Why this way:** they're "low-code" (generate real artifacts) rather than "metadata-interpreted at runtime" — different category, but it shows native RI is achievable when you own codegen.

### Retool / Appsmith / Budibase — *bind to your DB*
- These don't own the storage model — they connect to **your** Postgres/MySQL where **you** define the FKs. RI is whatever the underlying DB enforces. Not directly comparable, but reinforces that "let the DB do it" is the default expectation.

---

## 2. The four named patterns

| Pattern | Storage | RI mechanism | Examples | Tradeoffs |
|---|---|---|---|---|
| **A. Universal/shared table** | Wide tables + overflow KV | App-layer only | Salesforce | Max multitenant scale; weakest RI; hardest to query/join |
| **B. Real table/entity + real native FK** | Real table per entity | DB `FOREIGN KEY` | Odoo, Dataverse, OutSystems | Strongest RI; natural SQL/joins; needs real DDL + migration engine |
| **C. Real table/entity + soft references** | Real table per entity | App-layer, GUID columns | ServiceNow | Queryable like B; flexible relationship changes; weaker RI than B |
| **D. JSONB document per entity** | One table/entity, attrs in JSONB | App-layer (or hoisted columns) | *(many modern apps, variants)* | Flexible schema; can't FK inside JSONB unless you hoist |

**Critical insight:** Patterns B, C, D all use a "real table per entity." The difference is whether references are real typed columns with DB FKs (B) or loose values (C/D). The plan's §5.1 currently sits in D and therefore inherits C/A's *app-layer RI* obligation — whether it says so or not.

---

## 3. If you stay with app-layer RI (patterns A/C/D), here's how to do it well

You're signing up to re-implement the DB's job. Do it deliberately:

1. **Model relationship strength in metadata** (steal Salesforce's split):
   - `md_relationship.strength = master_detail | lookup`
   - `master_detail`: cascade delete, no orphan, ownership inherited, enables roll-up summaries.
   - `lookup`: configurable `on_delete ∈ {restrict, set_null, cascade}`.
2. **Build an inbound-reference registry from metadata.** For each entity E, compute "who references me and how" at metadata-load time. This makes cascade lookups O(1) instead of scanning every JSONB table.
3. **Optional: maintain a real reverse-index table** `_ref(source_entity, source_id, field, target_id)` updated by the write path (or a trigger). Lets cascade resolution be a single indexed query instead of table scans across entities.
4. **On delete of a referenced record**, in one DB transaction, for each inbound relationship apply its `on_delete`:
   - `restrict` → abort if any inbound ref exists.
   - `cascade` → recursively delete referencing records (depth cap + visited-set cycle guard).
   - `set_null` → `UPDATE … SET attributes = jsonb_set(...null) WHERE …`.
5. **Validate references on write** of the *referencing* record (existence + reference qualifier), but be aware of the race: between your `SELECT` existence check and `INSERT`, the target could be deleted. Mitigate with `SELECT ... FOR UPDATE` on the target, or `SERIALIZABLE`, or accept + reconciler job.
6. **Reconciler / orphan-sweep job** — periodic background scan that detects and repairs dangling references (log + optionally auto-clear). Defense in depth; app-layer RI will leak eventually.

> This is real, ongoing engineering surface area. The DB gives you all of it for free if you let it.

---

## 4. The soft-delete gotcha (affects all patterns)

Soft-delete (`deleted_at`) **breaks native cascade** because the row isn't actually deleted, so `ON DELETE CASCADE` never fires. You have three options:
- **Hard-delete + archive:** move the row to an archive table, let native FKs cascade. Cleanest for RI; loses easy "undo."
- **Soft-delete + app-layer cascade:** your runtime walks inbound refs and applies rules on the *logical* delete. Re-implements cascade in app code.
- **Partial indexes + no cascade:** references survive pointing at soft-deleted records; queries filter `WHERE target.deleted_at IS NULL`. Weakest.

Salesforce/ServiceNow lean toward option 2 (their rows persist with a deleted flag). Odoo/Dynamics lean toward hard-delete (option 1) because they have native FKs. **Decide this in Phase 0** — it's entangled with the RI strategy.

---

## 5. Other useful Postgres tricks
- **`DEFERRABLE INITIALLY DEFERRED`** FK constraints — let you have circular references (A↔B) and check integrity at commit, not per-statement. Essential for mutual references.
- **`ON DELETE SET NULL`** on nullable refs; **`ON DELETE RESTRICT`** to block.
- **Expression indexes** `CREATE INDEX ON biz.customer ((attributes->>'email'))` if you keep email in JSONB — needed for unique constraints and joins on JSONB-stored values.
- **Generated columns** `email TEXT GENERATED ALWAYS AS (attributes->>'email') STORED` — hoist a JSONB value into a real, queryable, indexable, *FK-able* column automatically. This is the bridge to the hybrid below.

---

## 6. Recommendation for MDA

**Don't reproduce Salesforce's constraint.** You are not running 100k tenants on shared hardware. You have no reason to give up native RI.

**Adopt Pattern B with a JSONB escape hatch — i.e., make the plan's "hybrid D" the *default from day one*, not a future target:**

- **Real table per entity** (`biz.<entity>`) created at publish time, with a stable core schema: `id, tenant_id, owner_id, state, version, created_at, updated_at` (no `deleted_at` — deletion is hard-delete + archive, [ADR-0006](adr/0006-deletion-hard-delete-and-archive.md)) plus **hoisted columns** for relational fields.
- **Reference fields are real typed columns with real `FOREIGN KEY` constraints** + `ON DELETE` from the relationship's declared behavior. Use `DEFERRABLE` where mutual references exist.
- **Plain scalar fields**: choose per-field whether to hoist to a real column (for indexed/queried/unique fields) or keep in an `attributes JSONB` payload. Use **generated columns** to hoist cheaply and keep a single source of truth.
- **Model relationship strength** in `md_relationship` (master-detail vs lookup + `on_delete`), mirroring Salesforce/Dataverse.
- **Own a real DDL engine** — but you need that anyway for C3 (data migration on schema change). The same engine that adds a column is the one that runs a data transform. RI and migration share an infrastructure; build it once.

**Concretely this means amending PLAN.md §5.1:** the "JSONB document per entity" recommendation becomes **"real table per entity: stable core + hoisted relational/indexed columns + JSONB `attributes` for the rest, with native Postgres FK constraints."** That single change:
- resolves REVIEW.md **C1** (RI) for free via the DB,
- collapses the artificial B-vs-D ambiguity,
- and makes the data-migration engine (C3) strictly necessary and therefore properly budgeted.

The cost — a DDL/migration engine on publish — is a cost you pay once and that pays for itself across every other subsystem (reports, RI, indexing, perf).

---

## TL;DR
- Salesforce = universal tables → app-layer RI (forced by multitenancy).
- Odoo / Dynamics = real tables → **native FK constraints** (strongest, chosen because they can).
- ServiceNow = real tables but soft refs → app-layer RI (middle ground).
- **For MDA: go the Odoo/Dynamics route.** Real table per entity + native Postgres FKs + JSONB for the non-relational remainder. Model master-detail vs lookup in metadata. Don't reimplement what Postgres already does unless you have Salesforce's problem — and you don't.
