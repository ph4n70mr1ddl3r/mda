# ADR-0024: GraphQL schema hot-invalidation

- **Status:** Accepted
- **Date:** 2026-01-28
- **Resolves:** the GraphQL "not hot-reloaded on invalidation" follow-up recorded
  in `docs/CAPABILITIES.md` under ADR-0020
- **Detail:** ADR-0010 / ADR-0020; implementation `crates/mda-api/src/graphql.rs`
  (`spawn_invalidator`, `invalidate_all`) + `crates/mda-server/src/lib.rs`

## Context

The dynamic GraphQL schema (ADR-0010) is cached per `(tenant, active_version)`.
A publish advances the version, so the next request rebuilds the schema — the
*behaviour* was always correct (no stale schema was ever served). The recorded
follow-up was memory hygiene: stale version entries were never evicted, so they
accumulated across many publishes, and the cache relied solely on the
version-key for freshness with no `meta_changed` hook of its own.

## Decision

Hook the GraphQL schema cache to the **same `meta_changed` Postgres
notification** that already invalidates the entity-definition cache (§5.3):
`spawn_invalidator` LISTENs on `meta_changed` and clears the schema store on
every notification. The `(tenant, version)` key remains the correctness guarantee
(a publish rebuilds by advancing the version); the LISTEN hook is now the
prompt-eviction + bounded-memory guarantee, mirroring the metadata cache's
two-tier invalidation (fast-path NOTIFY + version-stamp poll fallback).

## Consequences

- Bounded memory: stale version entries are evicted on publish, not retained
  for the process lifetime.
- Promptness: a schema rebuild happens on the first post-publish request (key
  miss) and stale entries clear on the NOTIFY — no admin action needed.
- Self-healing is unchanged: even a missed NOTIFY is harmless, because the
  version key means a stale entry is simply never read again.
