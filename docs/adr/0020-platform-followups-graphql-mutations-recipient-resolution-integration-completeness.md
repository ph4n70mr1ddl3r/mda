# ADR-0020: Platform follow-ups — GraphQL mutations, recipient resolution, integration completeness

- **Status:** Accepted
- **Date:** 2026-01-27
- **Resolves:** the remaining "follow-ups" recorded in `docs/CAPABILITIES.md`
  (Decisions/deferrals) under the §5.18–5.22 cluster and ADR-0010.
- **Detail:** PLAN §5.18 / §5.22 / §14; `docs/CAPABILITIES.md`;
  implementation across `crates/mda-api/src/{data,graphql,notifications,mail,
  schedules,integrations}.rs` and `crates/mda-integration/src/lib.rs`.

## Context

The §5.18–5.22 platform-capability cluster and GraphQL (ADR-0010) shipped
read/query-first, with a documented set of follow-ups: GraphQL mutations,
"notify everyone who can read this record" recipient resolution (ADR-0013
record-share materialization) + FLS-under-recipient email rendering, an SMTP
transport, and four integration refinements (`field_level_sor` conflict policy,
debatching, per-flow running user, cron-scheduled flow execution). Each is
generic (domain-neutral) and was the natural completion of an already-accepted
capability, so no new architectural direction is introduced — this ADR records
that the deferrals are closed and the design choices made while closing them.

## Decision

**GraphQL mutations (ADR-0010 parity).** Add `Mutation.create/update/delete
<Entity>` to the dynamic schema. The mutations call the **same** write service
as REST — `data::create_record_service` / `update_record_service` /
`delete_record_service` (factored out of the REST handlers so both surfaces
share one path). Consequence: RBAC, FLS write-check, rules, calculated fields,
audit, and OCC all fire identically; an `mda.<kind>` failure surfaces as a
GraphQL error carrying the `code` extension (the same key as the REST envelope).

**Notification recipients — record-share materialization (§5.18 / ADR-0013).**
`recipient_strategy: "record_readers"` on `POST /api/notifications/dispatch`
resolves the recipient set as "everyone who can read this record": the owner +
direct shares +, when the entity's OWD grants org-wide read, every active user
whose role grants object-level `read` (`mda_api::notifications::
resolve_record_readers`). Object-level read is always required (it is the gate
to read any record). Team-OWD is treated like `private` here; full
team-hierarchy traversal stays a deeper ADR-0013 refinement.

**FLS-under-recipient email rendering (§5.18).** The email channel
FLS-projects the render context per recipient before rendering: any `record`
field the recipient may not read is dropped, so a notification email can never
leak an unreadable field (`project_context_for_recipient`).

**Email transport — SMTP (§5.18).** A pluggable `MailSender` trait
(`mda_api::mail`) with a minimal SMTP relay client (`SmtpMailSender`,
env-configured `MDA_SMTP_*`, RFC-5321 dot-stuffing) and a safe `NoopMailSender`
default. The message is recorded in `sys_message` either way (audit + a retry
worker re-sends). TLS/SMTP-AUTH/`lettre` plug in behind the same trait.

**Integration — `field_level_sor` conflict policy (§5.22.4).** A per-flow
`config.sor_fields` list declares which canonical fields an external system
owns; on update the hub writes only those, preserving fields owned by other
systems. `last_write_wins` and `manual` are unchanged.

**Integration — debatching (§5.22.2).** A `debatch` flow step fans one inbound
payload (an array field) into N canonical records; the parent context (minus
the array field) propagates to each child. `run_inbound_batch` /
`fetch_and_run_inbound` load the flow's steps + value maps once and run each
expanded payload.

**Integration — per-flow running user (§5.22).** `int.flow.running_user_id`
makes newly created records owned by a scoped principal instead of a blanket
system superuser (the fallback when unset).

**Integration — scheduled (cron) execution (§5.22 / §14).** The scheduler's
`integration` kind pulls an inbound flow from its connector on cadence and
materializes the fetched records (`mda_integration::fetch_and_run_inbound`).
The scheduler worker now threads the secret store (connector auth) through,
mirroring the outbox drain.

## Consequences

- Two write paths no longer exist: REST and GraphQL share one service layer, so
  security/audit/rule behaviour cannot drift between them.
- "Notify everyone who can read this record" is a first-class dispatch mode,
  but it is an explicit opt-in (`recipient_strategy`) — the default remains
  explicit recipients (the rule/workflow knows who: assignee/owner/named user),
  which avoids an implicit tenant-wide fan-out footgun.
- The SMTP client ships as a plain relay hop (no TLS/AUTH); an internet-facing
  relay needs the TLS/AUTH impl behind the same `MailSender` trait (still a
  follow-up, now a narrow one).
- `int.flow` gains two nullable/defaulted columns (`running_user_id`,
  `config`); existing flows keep their previous behaviour.
