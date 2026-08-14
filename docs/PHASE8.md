# Phase 8 — Studio UI (status & handoff)

**Status: complete.** The Phase-8 deliverable — *"a business analyst can build
a small CRM app entirely through the browser"* — is now real: the Leptos
Runtime UI grows an admin-gated **Studio** (header link, visible after
`/api/auth/me` reports `is_superuser`) that covers every authoring surface the
platform has, and the two surfaces that were missing (rules, workflows) ship
as new authoring APIs first.

Design stance: the Studio is **thin** — it holds no security or validation
logic of its own. Every mutation goes through the admin-gated APIs, which
validate against the active model and enforce AuthZ; the UI renders what the
server says. (The drag-and-drop form/flow painters of the original sketch are
a v2 aesthetic layer on top of these same contracts — the metadata they would
produce is already fully authorable here.)

## New authoring APIs (the gaps the designers needed)

- **Rules** (`mda-api/src/rules.rs`, Phase-4 engine now operable):
  `GET/POST /api/rules`, `PATCH/DELETE /api/rules/:id`. Author-time validation:
  event ∈ {before,after}×{create,update}, entity + action field resolve
  against the active model, condition + action value parse as bounded-DSL
  expressions (a typo fails at author time, not on every write).
  `--test rules_workflows`: bad event / unknown field / unparseable condition
  → 422; a valid `status = Closed → closed_at = now()` rule actually fires on
  a PATCH; deactivate stops it; delete removes it.
- **Workflows** (`mda-api/src/workflows.rs`, Phase-5 engine now operable):
  `GET/POST /api/workflows`, `PATCH/DELETE /api/workflows/:id`. A machine is
  authored as one unit (entity, name, states, transitions) in one transaction
  — states unique, transitions connect declared states, guards parse, action
  fields resolve — or nothing is stored. The test authors a guard+action
  machine end-to-end: create → `close` (guard passes, action stamps
  `closed_at`) → `reopen`, then deactivation makes transitions 404, delete
  cascades states/transitions.
- **Draft lifecycle completions** (`studio.rs`): `GET /api/studio/drafts`
  (list, model blob elided) so prior drafts can be reopened, and
  `DELETE /api/studio/drafts/:id` (discard, `draft`-status only — a published
  draft is history). Also removed a duplicated validation call in the
  share-rules create path.

## The Studio (web/runtime-ui/src/studio.rs)

Six tabs, all riding existing + the new APIs:

- **Model** — the entity/field designer over the draft lifecycle: list/create/
  open/discard drafts (each branches the active model); edit the draft model
  in the browser (add entities; add fields with type/required/unique/indexed/
  enum options; add references with target + on-delete — a reference is the
  real FK column, §5.7); artifacts already active render **locked** with an
  `active` badge (an edit is a Phase-2 transform) and can only be *retired*
  (two-phase, allowed); **Save** (If-Match etag, server diff report shown),
  **Validate** (DiffReport: publishable flag, additions/retirements counts,
  violations/errors/warnings), **Save + Publish** (result summary; refreshes
  the shell's model + navigation).
- **Pages** — form designer (field picker with label/widget overrides,
  section title), view designer (column picker + page size), dashboard
  designer (report tiles), navigation designer (entity/link items). Upsert +
  delete per definition.
- **Reports** — author (name, base entity, select fields with aggregates +
  aliases, reference traversals like `customer.name`, filters, group-by,
  limit), run (result table), delete.
- **Automation** — rule editor (entity, event, condition builder
  always/field-op-value, set-field action with typed literals incl. `now()`)
  and workflow designer (states, transitions with from/to selects, guards,
  on-run set-field actions, creates-task), with activate/deactivate/delete.
- **Security** — teams (create, re-parent, delete), roles (create, grant/revoke
  object + field permissions, role-hierarchy parents), org-wide defaults per
  entity, users (create, team, disable, role assign/revoke, password reset),
  and criteria-based sharing rules (create with condition builder, toggle,
  delete — with the ADR-0013 epoch semantics surfaced in the messages).
- **Data** — export the active model as JSON, import a bundle **as a draft**
  (never publishes directly), and the publish-snapshot history.

Client plumbing: `/api/auth/me` gates the Studio link; a generic
auth+error-extracting JSON client (`api::sget/spost/spatch/sput/sdelete`) with
the draft save keeping its OCC `If-Match` header; client-side UUIDs via
WebCrypto for new draft artifacts.

## Verification

- `--test rules_workflows` (new): both authoring APIs validated + proven to
  drive the engines; `--test studio` grows `drafts_list_and_discard`
  (list w/o model blob, discard, published → 409, unknown → 404).
- `cargo clippy --all-targets --all-features -- -D warnings` clean;
  `trunk build --release` clean (CI's `ui` job builds the same bundle).
- Full DB-backed suite (`make test-db`) green.

## Phase-8 decisions / deferrals

- **No client-side security**: the Studio never decides visibility — it calls
  admin APIs and renders the verdict. A 403 on a non-admin hides the Studio
  link entirely (`is_admin` from `/api/auth/me`).
- **Workflow edits are replace, not patch**: the machine is a unit (graph
  integrity is validated as a whole); in-place transition surgery invites
  half-edited machines. Delete + recreate (or deactivate).
- **Active artifacts lock, retire only** in the model designer — matching the
  additive-only publish; the diff report still catches anything that slips
  through (e.g. a hand-edited import bundle).
- Deferred: drag-and-drop layout painters, undo/redo, multi-user draft
  presence (the OCC etag already prevents lost updates), template/rules
  libraries.
