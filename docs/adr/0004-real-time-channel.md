# ADR-0004: Real-time channel (SSE over the event log)

- **Status:** Accepted
- **Date:** 2025-07-16
- **Resolves:** REVIEW.md C5
- **Detail:** PLAN.md §5.10

## Context
A metadata-driven UI collapses into stale data unless changes are pushed (another user edits the same record, a workflow silently moves it). Polling a dynamic API is a non-starter at scale and a UX disaster.

## Decision
Server-Sent Events, sourced from the canonical `sys_event_log` stream (written transactionally with each change — ADR-0003). Per-instance relays fan out by subscription channel; Postgres `NOTIFY` spans instances (no Redis needed for the core hop); `Last-Event-ID` replay from `sys_event_log` makes delivery reliable across reconnects. AuthZ — including field-level visibility — is enforced per-client at the relay.

## Consequences
- **(+)** Reliable push with no new source of truth (reuses the event log).
- **(+)** Conflict banners complement OCC (ADR-0003) so users see stale-data conflicts *before* saving.
- **(−)** WebSocket + OT/CRDT for true collaborative co-editing is deferred to v2.
