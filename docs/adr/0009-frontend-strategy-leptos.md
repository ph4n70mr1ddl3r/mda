# ADR-0009: Frontend strategy — Leptos (Rust/WASM)

- **Status:** Accepted (supersedes the Phase-0 "decide via spike" state)
- **Date:** 2026-07-16
- **Resolves:** REVIEW.md §8 (Leptos hedge); Phase 8 timeline risk
- **Detail:** PLAN §8, Phase 0

## Context

The drag-and-drop Studio designers are the single highest-risk, highest-effort
component. ADR-0009 deferred the Leptos-vs-React decision to a Phase-0 spike:
build a throwaway metadata-driven form renderer in **both** frameworks, evaluate
on evidence, and record the choice.

Both renderers were built and **both build cleanly** (`web/spike-leptos` via
Trunk, `web/spike-react` via Vite). The evaluation is complete.

## Decision

**Adopt Leptos (Rust/WASM, CSR)** for both the Runtime UI and the Studio UI.

## Rationale

- **Full-stack Rust** — shares `serde` types natively with the backend; one
  language, one toolchain, one mental model across the entire platform.
- **Type safety end-to-end** — entity/field/model types defined once in
  `mda-meta`, shared with the frontend (via `serde_wasm_bindgen` or a shared
  crate), eliminating the ts-rs codegen step React would require.
- **The Runtime UI is logic-focused** — a metadata-driven form/table renderer is
  mostly logic (field-type → input mapping, conditional visibility, OCC conflict
  handling), not visual flair. Leptos excels at this.
- **Acceptable ecosystem** — the Leptos component ecosystem is growing; for the
  Studio drag-and-drop designers, investment in Leptos is the calculated bet.
  If a specific designer component proves out of reach, a targeted escape hatch
  (embedded React widget) is possible without changing the overall decision.

## Consequences

- **(+)** Complete all-Rust stack; shared types; no TypeScript codegen.
- **(+)** The throwaway spike (`web/spike-leptos`) evolves directly into the
  production Runtime UI — no wasted work.
- **(−)** Niche ecosystem vs React; hiring and component availability are thinner.
- **(−)** WASM bundle size (~1.6 MB dev) larger than a React equivalent; acceptable
  for an enterprise internal tool, mitigated by release-mode optimization.
- The React spike (`web/spike-react`) is retained as a reference but not used.
