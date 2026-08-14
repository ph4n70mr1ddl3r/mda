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
- `Mutation.create/update/delete<Entity>` (now implemented — reads + writes reach
  REST parity): each mutation calls the **same** write service as REST
  (`create/update/delete_record_service`) so RBAC + FLS write-check + rules +
  calculated fields + audit all fire identically. OCC conflicts and AuthZ denials
  surface as GraphQL errors carrying the stable `code` extension (`mda.conflict`,
  `mda.forbidden`, …) — the same keys as the REST envelope.
- Depth + complexity limits deny expensive nested queries (§5.17).

## Verification

- Unit: secrets n/a (trait); templating (6); expression (6); integration (7);
  mail (3).
- DB-backed (each its own fresh database, fully parallel):
  secrets (2) · templates (2) · notifications (6) · webhooks (4) ·
  integration_flows (6) · graphql (5) · scheduler (4) · tenants (3). Plus the
  full prior suite green, incl. a team-OWD record-visibility test in `data`.

## Decisions / deferrals

The earlier follow-ups are now **implemented** (each with a DB-backed test):

- **Notification recipients — “notify everyone who can read this record”** is
  now resolvable. `recipient_strategy: "record_readers"` on
  `POST /api/notifications/dispatch` resolves the owner + direct shares +, when
  the entity's OWD grants org-wide read, every active user whose role grants
  object-level `read`, and, under team-OWD, the owner's teammates
  (ADR-0013 record-share materialization).
  `resolve_record_readers` is the reusable helper (call from a rule/workflow).
- **FLS-under-recipient for email rendering** — the email channel now FLS-
  projects the render context per recipient (`record` fields the recipient may
  not read are dropped before the template renders), so a notification email can
  never leak an unreadable field.
- **Email transport (SMTP send)** — a pluggable `MailSender` boundary
  (`mda-api::mail`) with a minimal SMTP relay client (`SmtpMailSender`,
  env-configured `MDA_SMTP_*`) and a safe `NoopMailSender` default. The message
  is recorded in `sys_message` either way (audit + a retry worker re-sends).
  TLS/SMTP-AUTH/`lettre` plug in behind the same trait.
- **Integration `field_level_sor` conflict policy** — a per-flow `config.sor_fields`
  list declares which canonical fields an external system owns; on update the
  hub writes only those, preserving fields owned by other systems.
- **Integration debatching** — a `debatch` flow step fans one inbound payload
  (an array field) into N canonical records (the parent context propagates).
- **Integration per-flow `running_user`** — `int.flow.running_user_id` makes new
  records owned by a scoped principal instead of a blanket system superuser.
- **Integration scheduled (cron) execution** — the scheduler's `integration`
  kind pulls an inbound flow from its connector on cadence and materializes the
  fetched records (`fetch_and_run_inbound`; the same path a webhook trigger uses).

Remaining, still-deferred (lower priority / tied to other work):

- **GraphQL schema hot-invalidation** is now **closed** (ADR-0024): the schema
  cache is hooked to the `meta_changed` NOTIFY (same channel as the metadata
  cache) so stale version entries are evicted on publish, not retained. The
  `(tenant, version)` key remains the correctness guarantee (a publish rebuilds
  by advancing the version).
- **Team hierarchy** (parent-team / sub-team visibility in record security +
  recipient resolution). Flat **team-OWD is now honored** (ADR-0013
  `owd_visible`): an entity whose OWD is `team` admits the owner's teammates
  for read (write stays owner-only, mirroring `PublicRead`), in both the
  record-visibility predicate and `record_readers` recipient resolution. The
  remaining refinement is the *hierarchy* (a manager's team sees descendant
  teams' records), which needs the role/team-hierarchy epoch machinery.
- **Tenant-scoped *data* export/restore + data residency** remain tied to the
  tenant lifecycle (§5.4) and HA (U9). Tenant *configuration* export **and**
  import now ship: `GET /api/tenants/export` + `POST /api/tenants/import`
  (merge-by-natural-key with FK id-remapping, idempotent; the model stages as a
  Studio draft). Full record-data export/restore + regional placement stay tied
  to tenant lifecycle.
