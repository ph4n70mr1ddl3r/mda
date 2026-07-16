# Phase 5 — Workflow Engine (status & handoff)

**Status: complete & verified.** Implements PLAN §9 Phase 5: state-machine
workflows over entities with guard-checked transitions, set-field actions, task
creation, and outbox-enqueued side-effects — all in the write transaction
(ADR-0016 sync-by-default). Deliverable met: *an approval workflow on an
"Invoice" entity with state, tasks, and a transitioned event.*

## What was built

**Tables** (`migrations/20260105000001`): `md_workflow`, `md_workflow_state`,
`md_workflow_transition` (guard + JSON actions + `creates_task`),
`md_workflow_task`, and `sys_outbox` (the transactional outbox; drain worker is
a follow-up).

**`mda-workflow`** (PLAN §4.3 / §5.9.5): `run_transition` —
1. resolve the active workflow + the record's current `state` (core column);
2. find the named transition from that state; **evaluate its guard** (DSL);
3. apply the transition's set-field actions, fire `after_update` rules + calculated
   fields, then persist via `mda_data::update(..., new_state = Some(to_state))`
   — OCC + write scope, in-transaction;
4. create a `md_workflow_task` if the transition requires one;
5. enqueue a `workflow.transitioned` row in `sys_outbox`.

**`mda_data::update`** gained an optional `new_state` (the core `state` column).

**API**: `POST /api/data/:entity/:id/:transition` (`If-Match: <version>`).

## Verification (all green)

`fmt` · `clippy -D warnings`; `--test data workflow_state_machine_runs`:
create (state `active`) → `submit`→`Submitted` (**approval task created**) →
`approve`→`Approved` (guard `amount > 0`; action sets `approved_at = now()`;
**outbox rows** for both transitions); a zero-amount invoice's `approve` is
**rejected by the guard (422)**.

## Phase-5 decisions / deferrals

- Transitions gate on object `update` permission for Phase 5 (action-level
  `sec_action_permission`, §5.11 grain 5, is a refinement).
- **Async timers** (apalis-scheduled SLA/escalation with `FOR UPDATE`
  serialization, §5.9.6) and **async chained transitions** (ADR-0016) are
  follow-ups; sync in-transaction transitions are implemented.
- The **outbox drain worker** (webhook/email/notification delivery) and
  **notifications** (Phase 5/6) are not yet wired — the table + enqueue are.
- Workflow authoring UI is the Studio (Phase 8); workflows are inserted via
  metadata for now.
