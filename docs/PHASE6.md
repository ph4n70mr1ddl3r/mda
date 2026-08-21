# Phase 6 — Form & view definitions + Runtime UI (status & handoff)

**Status: complete & verified.** Implements PLAN §4.2 (UI definitions) and the
v1 scope of §5.10 (real-time channel): the four definition tables with render
APIs resolved against the **active model + the caller's security**, and the
Leptos Runtime UI rendering from them (zero hardcoded pages).

## What was built

**UI definitions** (`meta.md_form` / `md_view` / `md_dashboard` /
`md_navigation`; API surface and semantics in `docs/CAPABILITIES.md` →
"UI definitions"):

- `GET /api/forms/:entity[?name=default]` — renderable form: sections with
  ordered fields (name, label, type, required, widget, options, and
  `target_entity` for reference pickers). No stored form → a default
  synthesized from the field registry. **FLS-projected per caller**: a field
  the caller cannot read is dropped from the payload.
- `GET /api/views/:entity[?name=default]` — renderable grid (columns with
  labels/types, default filters, sort, page size), FLS-projected; unknown
  columns are rejected at author time. `POST/DELETE` to manage.
- `GET /api/dashboards[/:id]` — definitions; `:id` **runs each report under
  the requesting identity** (object/field/record security per run — a
  dashboard is a saved lens, not a stored result set). Broken tiles render an
  inline `error`, never a 500.
- `GET /api/navigation` — the caller's permission-filtered menu (unreadable
  entities never appear; authored labels win; external items are http(s)
  links only).

**Real-time channel (§5.10 v1 scope):** `GET /api/events` — SSE over
`sys_event_log` with `Last-Event-ID` replay; authenticated via a bearer header
or a short-lived ticket (`POST /api/auth/event-ticket`, since `EventSource`
cannot set headers); per-subscription channel AuthZ; the write path emits
`record.created/updated/deleted`, workflow, and notification events.

**Runtime UI** (`web/runtime-ui`, Leptos CSR/WASM): login, navigation shell,
view-driven grids (filter/sort/paging), form-definition-driven editors
(incl. reference pickers resolved from the target entity), dashboards, and
the **real-time conflict banner** — an incoming `record.updated` for the
record being edited offers Review/Overwrite/Refresh *before* a wasted save;
the 409-on-save OCC backstop (§5.9) remains underneath.

## Verification

DB-backed (each with its own fresh database, fully parallel): `ui_defs` (4) —
default-then-authored forms + FLS projection, view author validation + FLS
drop, dashboards running reports under the caller, permission-filtered
navigation; `events` — SSE auth (missing/invalid token rejected; bearer and
short-lived ticket accepted). Plus the full prior suite green.

## Phase-6 decisions / deferrals

- Per §5.10.9, deferred items stay deferred: live list/kanban/count
  streaming, presence heartbeats, and collaborative co-editing (needs
  WebSocket + OT/CRDT → v2).
- The advanced record-security bullet (deferred from Phase 3) closed later
  via the sharing-rules materialization arc — ADR-0013 / ADR-0026 (see
  PLAN §9 Phase 3 note).
