# ADR-0010: GraphQL as a first-class runtime data API

- **Status:** Accepted
- **Date:** 2025-07-16
- **Resolves:** REVIEW.md §7 (GraphQL dismissal)
- **Detail:** PLAN.md §7, §6

## Context
The runtime data API is dynamic and relationship-rich. REST is fine for simple CRUD, but clients traversing references (customer → invoices → line items) require multiple round-trips or bespoke endpoints. GraphQL was previously dismissed as "optional."

## Decision
Make **GraphQL (`async-graphql`) a first-class runtime data API**, running alongside REST (which stays for Studio, auth, and SSE). The schema is generated from the active model and re-generated on publish. AuthZ and field-level security apply per field. Prototype in Phase 2 and enforce query depth/cost limits to deny expensive nested queries.

## Consequences
- **(+)** Clients fetch exactly the related data they need in a single request — a strong fit for a model-driven API.
- **(+)** Schema is self-documenting and derived from metadata.
- **(−)** Two API surfaces to maintain (mitigated: both sit on the same service layer).
- **(−)** Must guard against expensive nested queries (depth/cost limits).
