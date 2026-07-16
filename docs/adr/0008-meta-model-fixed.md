# ADR-0008: Meta-model — fixed, not self-hosting

- **Status:** Accepted
- **Date:** 2025-07-16
- **Resolves:** REVIEW.md U8
- **Detail:** PLAN.md §5.12, §4

## Context
Salesforce makes its own model first-class (`EntityDefinition` is itself an editable entity). Self-hosting the meta-model is elegant but introduces bootstrapping and an infinite-regress of meta-meta-(meta-)tables, adding large complexity for modest v1 gain.

## Decision
The meta-model (`md_entity`, `md_field`, `md_relationship`, …) is **fixed Rust structs + SQL**, edited by dedicated Studio handlers — not first-class runtime entities.

## Consequences
- **(+)** No bootstrapping / infinite-regress; a simpler Studio.
- **(−)** "No edit the editor" — the platform's own definitions cannot be redefined at runtime.
- **(−)** Custom entity/field types remain extensible only via the registry + `wasmtime` (§5.6). Revisit if a use case demands user-defined meta-types.
