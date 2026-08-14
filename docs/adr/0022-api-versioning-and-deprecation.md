# ADR-0022: API versioning & deprecation

- **Status:** Accepted
- **Date:** 2026-01-28
- **Resolves:** PLAN §9 "API versioning" (was a tracked, unscheduled deferral)
- **Detail:** PLAN §7 / §9; implementation `crates/mda-api/src/versioning.rs`

## Context

§9 tracked "API versioning — REST + GraphQL versioning/deprecation strategy for
generated SDK clients (§7): breaking-change management via path/header
versioning, sunset headers, and parallel schema generations across publishes" as
an open deferral. A metadata-driven API generates SDK clients from the active
model; without a versioning contract a breaking model change (a renamed field,
a removed entity) is a silent break for every client. The platform needs a way
to (a) let a client pin a major, (b) advertise which major it actually got,
and (c) deprecate + eventually retire an old major with explicit, machine-
readable warning ahead of removal.

## Decision

Ship a **versioning + deprecation middleware** (`versioning::middleware`)
applied innermost so it reaches every route — REST and GraphQL alike:

- **Negotiation.** The requested major is read from an explicit
  `X-API-Version: <n>` header, or an
  `Accept: application/vnd.mda+json; version=<n>` vendor media-type parameter.
  Absent → the current stable major. We do *not* silently upgrade an
  explicitly-pinned client (that is a breaking lie); a pinned request for an
  unsupported major is rejected.
- **Discovery.** Every response carries `MDA-API-Version: <served>` so an SDK
  can detect which major it actually received.
- **Deprecation (RFC-8594).** When a newer major is current, an older (still-
  served) major is *deprecated*: requests pinning it get `Deprecation: true`,
  `Sunset: <date>`, and `Link: <doc>; rel="deprecation"` so SDKs warn + migrate
  ahead of removal.
- **Sunset enforcement.** A major older than the floor
  (`MDA_MIN_API_VERSION`) is *unsupported* → `400` with a stable
  `mda.unsupported_version` code (`{ requested_version, minimum_supported_version }`)
  plus `Sunset`/`Link`.

The policy is **config-driven, not code-driven**: `MDA_API_VERSION` (current),
`MDA_MIN_API_VERSION` (floor), `MDA_DEPRECATED_VERSIONS` (comma list),
`MDA_SUNSET_DATE`, `MDA_DEPRECATION_LINK`. Only major `1` ships today; a future
`v2` cutover is an env change (`MDA_API_VERSION=2`,
`MDA_DEPRECATED_VERSIONS=1`), not a code change. Parallel-major schema
generations across publishes (§7) slot in behind the same negotiation boundary
when a v2 schema diverges.

## Consequences

- One negotiation boundary serves both REST and GraphQL — no per-route version
  plumbing.
- Deprecation is observable: SDKs can surface "this major is sunset on <date>,
  migrate" without scraping changelogs.
- The `mda.unsupported_version` code joins the stable error taxonomy
  (ADR-0018) as the SDK branch key for the 400.
- This ships the *machinery*; concrete parallel-major schema generation is a
  later, v2-triggered step. The env knobs mean flipping current/deprecated/floor
  at cutover is operational, not developmental.
