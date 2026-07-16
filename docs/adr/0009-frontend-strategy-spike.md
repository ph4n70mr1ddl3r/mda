# ADR-0009: Frontend strategy — decide via a Phase 0 spike

- **Status:** Accepted
- **Date:** 2025-07-16
- **Resolves:** REVIEW.md §8 (Leptos hedge); Phase 8 timeline risk
- **Detail:** PLAN.md §8, Phase 0

## Context
The drag-and-drop Studio designers are the single highest-risk, highest-effort component. Committing now to all-Rust (Leptos) vs React/TS — without evidence — risks a costly late reversal in Phase 8.

## Decision
Defer the Studio-technology decision to a **Phase 0 spike**: build a throwaway metadata-driven form renderer in *both* Leptos and React, evaluate ergonomics / ecosystem / type-sharing, and record the choice. The Runtime UI will likely follow the same decision.

## Consequences
- **(+)** De-risks the biggest component on evidence, not guesswork.
- **(−)** Adds ~1 week of throwaway work in Phase 0 (runs in parallel with infra setup). Accepted.
- **(−)** Until the spike completes, the frontend stack is provisional.
