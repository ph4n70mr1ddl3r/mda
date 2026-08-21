# Phase 9 — Integration layer (status & handoff)

**Status: complete & verified (core).** Implements PLAN §5.22 — the **hub**
model: no stateless A→B pass-through; every flow materializes into the
platform's canonical `biz.*` entities, so AuthZ, audit, rules, and workflows
apply *between* systems.

## What was built

- **`mda-integration` crate:** `int.connector` / `int.flow` / `int.flow_step`
  / `int.value_map` (RLS-gated definitions), the `int_external_id`
  correlation registry, and `sys_integration_run` history.
- **`Connector` trait** + the universal **HTTP** transport (bearer/header/
  basic auth resolved server-side from the SecretStore, §5.20 — credentials
  never appear in metadata, logs, or events). DB/file/MQ/GraphQL/SOAP
  transports and niche formats (EDI, IDoc, AS2/OFTP) remain **extension
  connectors** on the same trait (§5.6/§5.22.6).
- **Inbound flows:** bounded expression-engine transform steps (transform /
  `value_map` / filter), **debatching** (one payload → N records), and
  upsert-by-external-key under a declared conflict policy —
  `last_write_wins` (with OCC retry), `field_level_sor` (per-field ownership
  via `config.sor_fields`), `manual` (quarantine for a human) — with a
  per-flow `running_user` so new records are owned by a scoped principal
  rather than a blanket superuser.
- **Outbound flows:** push mapped records through the connector.
- **Scheduling:** cron pulls via the ADR-0019 scheduler (`integration` kind)
  — `fetch_and_run_inbound`, the same path a webhook trigger uses.
- **Webhooks (§5.21):** versioned HMAC-signed envelope
  (`X-MDA-Signature: t=,v1=` with a replay window), at-least-once delivery
  via the outbox + DLQ, idempotent on `(webhook, event_id)`, replay
  endpoint, and a relay worker with a high-water cursor. The **inbound
  receiver** (`/api/integrations/webhooks/:id`) verifies the same signature
  (constant-time), resolves the tenant via a SECURITY DEFINER lookup,
  dedupes on `event_id`, and enqueues the inbound flow.
- **API:** `/api/connectors`, `/api/flows[/:id]` CRUD, manual run
  (`POST /api/flows/:id/run`), run history (`GET /api/flows/:id/runs`), and
  external-id lookup (`GET /api/external-ids/:entity/:key?system=`).

## Verification

DB-backed (own fresh database each): `integration_flows` (6) — inbound upsert
by external key (re-delivery does not duplicate), outbound push, value-map
translation, webhook → inbound flow via the outbox drain, `field_level_sor`
preserving non-owned fields, debatch materialization. Plus `webhooks` (4) —
signing, delivery, replay, inbound verification. Unit: integration (7).

## Phase-9 decisions / deferrals

- Transports beyond HTTP and the `source_priority` conflict policy are
  follow-ups on the same `Connector` / policy boundaries — no new
  architecture required.
- Large batch ETL rides the same resumable-job pattern as the deferred async
  `sys_impex_job` worker (§5.13 / Phase 10) when a flow outgrows the
  synchronous path.
