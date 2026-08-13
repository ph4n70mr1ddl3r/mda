# Platform capabilities (§5.18–5.22) + GraphQL (ADR-0010) — status & handoff

**Status: complete & verified.** Implements the platform-capability cluster the
plan scoped as design sections (§5.18–5.22) plus the first-class GraphQL runtime
API (ADR-0010). All are generic (domain-neutral) and share REST's service layer,
so object / field / record security applies by construction on every surface.

## Secrets (§5.20)

- `sys_secret` (reference only: name, kind, ref) + `sys_secret_audit` (every
  resolution: who/when/which/purpose). Values live in an external `SecretStore`.
- `mda-core::SecretStore` trait + `mda-api::LocalSecretStore` (env var → JSON
  file at `MDA_SECRET_FILE`). Cloud KMS / Vault impls are follow-ups (same trait).
- `resolve_and_audit` is the only path values are touched (server-side, under
  connector/channel authz). **Values are never returned by any API, never
  logged, and never serialized into events/audit/outbox payloads.**
- API: `POST/GET/DELETE /api/secrets[/:name]`, `POST /api/secrets/:name/rotate`.

## Templating (§5.19)

- `meta.md_template(name, kind[email|document|message], body, content_type, locale)`.
- `mda-reports::template` — sandboxed DSL: `{{ field }}` / `{{ path.to }}` /
  `{{ <JSON DSL expr> }}`, reusing the bounded expression engine (no code/I/O/SQL),
  capped by `MAX_INTERPOLATIONS`. 6 unit tests.
- **AuthZ-by-construction:** record-mode render loads through the data API
  (record-scope + field-level projection), so a template can never emit an
  unreadable field. Locale best-match (exact → prefix → default → any).
- API: `POST/GET/DELETE /api/templates[/:name]`, `POST /api/templates/:name/render`.

## Notifications & messaging (§5.18)

- `meta.md_notification_type` (opaque key, default channels, template link,
  digestible) + `sys_notification_preference` (per-user mute/opt-out, honored at
  fan-out) + `sys_message` (delivered email/message log).
- Pluggable `Channel` trait: `InAppChannel` (writes `sys_notification` + emits a
  `notification.created` event the SSE relay fans out) + `EmailChannel` (renders
  the §5.19 template, resolves addr from `sec_user`, records `sys_message`).
- `fanout` resolves the type, applies per-user preferences (a muted type is never
  produced), and delivers to each effective channel. `dispatch()` enqueues a
  transactional `notification.fanout` outbox row (call from rule/workflow paths).
- Digest sweep rolls a digestible type's unread batch into one summary.

## Event & webhook contract (§5.21) + inbound verification (§14)

- `int.webhook` subscriptions (event_types, entity_filter, secret_ref →
  SecretStore). Versioned HMAC-signed envelope `{event_id, tenant_id,
  schema_version, type, …}`; `X-MDA-Signature: t=,v1=` with a replay window.
- Delivery via the outbox (at-least-once + DLQ); idempotent on (webhook,
  event_id). Relay worker: `sys_event_log` → enqueues `webhook.deliver` for
  matching subs (high-water cursor). Replay endpoint (`?from=<event_id>`).
- Inbound receiver (`/api/integrations/webhooks/:id`) verifies the same signature
  (constant-time), via a SECURITY DEFINER `mda.lookup_webhook` that resolves the
  tenant (int.webhook is RLS-gated); dedupes on event_id; enqueues
  `integration.inbound` for the flow runner.

## Integration architecture (§5.22 / Phase 9)

- New crate `mda-integration`. `int.connector` / `int.flow` / `int.flow_step` /
  `int.value_map` (RLS-gated definitions) + `int_external_id` correlation
  registry (operational) + `sys_integration_run`.
- `Connector` trait + universal HTTP transport (auth resolved server-side from
  the SecretStore — bearer/header/basic); extension transports/formats are add-ons.
- Hub-model flow runners: inbound (materialize into `biz.*` via `mda-data`,
  bounded expression-engine transform steps — transform/value_map/filter — then
  upsert-by-external-key with a declared conflict policy: `last_write_wins` with
  OCC retry, `manual` → quarantine) and outbound (push through the connector).
- API: connectors/flows CRUD, manual run, external-id lookup, run history. The
  outbox drain routes `integration.inbound` (webhook receiver → inbound flow).

## GraphQL (ADR-0010)

- Dynamic schema (`async-graphql` dynamic) generated from the active model,
  cached per `(tenant, active_version)` so a publish rebuilds it. Each entity →
  an Object type with scalar fields + nested reference traversal.
- `Query.<entity>(id)` + `Query.<entity>s(first)`: reads via the same service
  layer as REST → object/field/record AuthZ by construction (needs `read` else
  nothing returned; FLS projects unreadable fields; ownership/OWD predicate
  injected; a nested reference loads only if the caller can read the target).
- Depth + complexity limits deny expensive nested queries (§5.17).
- MVP scope (ADR-0010): **query/traversal-first**; mutations reach REST parity
  progressively (clients mutate via `/api/data/:entity/*` for now).

## Verification

- Unit: secrets n/a (trait); templating (6); expression (6); integration (5).
- DB-backed (each its own fresh database, fully parallel):
  secrets (2) · templates (2) · notifications (4) · webhooks (4) ·
  integration_flows (4) · graphql (3). Plus the full prior suite green.

## Decisions / deferrals

- **Notification recipients are explicit** in `dispatch` (the rule/workflow knows
  who — assignee/owner/named user). Full "notify everyone who can read this
  record" (record-share materialization, ADR-0013) + FLS-under-recipient for
  email rendering are follow-ups.
- **Integration running_user** uses a system principal (superuser scope); a
  per-flow `running_user` with scoped AuthZ is a follow-up. SMTP send + scheduled
  (cron) flow execution (apalis) are follow-ups (event/webhook-triggered is the
  v1 path); debatching/batching steps + `field_level_sor` conflict policy land
  with them.
- **GraphQL** returns reads; mutations + GraphQL-side FLS-under-different-recipient
  are progressive. Schema is cached per version (not hot-reloaded on invalidation).
