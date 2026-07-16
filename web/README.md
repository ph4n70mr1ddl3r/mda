# Frontend spike (Phase 0, ADR-0009)

The drag-and-drop Studio designers are the highest-risk, highest-effort
component in the whole plan. Before committing to a stack, Phase 0 builds a
**throwaway metadata-driven form renderer in both** candidates so the call is
made on evidence, not guesswork (ADR-0009, PLAN §8).

Both renderers take a JSON form definition and map field types to inputs — the
core job of the future Runtime UI. There is **no backend form API yet**, so each
uses a local stub definition (`sampleForm`). They both probe `/health` to show
backend connectivity (run the Rust server first).

## Candidates

| | `spike-leptos/` | `spike-react/` |
|---|---|---|
| Stack | Leptos 0.6 (CSR) + Trunk → WASM | React 18 + TypeScript + Vite |
| Type-sharing with backend | Native (share `serde` types later) | Codegen (`ts-rs`) later |
| Build | `trunk build` / `trunk serve` | `npm run build` / `npm run dev` |
| Ecosystem | Growing, thinner | Huge |

## Run

```bash
# 1. start the API (serves /health; form API comes in Phase 1+)
cd ../.. && docker compose up -d postgres redis
DATABASE_URL=postgres://mda:mda@127.0.0.1:5433/mda?sslmode=disable cargo run

# 2a. React
cd spike-react && npm install && npm run dev      # http://localhost:5173

# 2b. Leptos   (needs: rustup target add wasm32-unknown-unknown; trunk)
cd spike-leptos && trunk serve                     # http://localhost:8080
```

> `spike-leptos/` is a **standalone** Cargo project (empty `[workspace]` table)
> so it stays out of the parent Rust workspace.

## What to evaluate (record the decision as an ADR)

- **Ergonomics** of a metadata → input mapping in each framework.
- **Type safety / sharing** with the Rust backend (Leptos shares `serde` types
  natively; React needs `ts-rs` codegen).
- **DX/build time** and the component ecosystem for the heavier Studio
  designers (drag-and-drop, grids, trees) that come in Phase 8.
- **Bundle size / runtime**: WASM (~1.6 MB `.wasm`, unoptimized dev) vs JS
  (~145 KB minified).

The Runtime UI will likely follow whatever is chosen for the Studio. This spike
does **not** decide the question — it produces the evidence. Both builds are
verified passing in Phase 0.
