# Load Test — Phase 11 first pass (2026-08-14)

Methodology: the **release** binary, booted in the production configuration
(`MDA_APP_DATABASE_URL` → the non-superuser `mda_app` role, so `biz.*` RLS
engages; `MDA_DB_MAX_CONNECTIONS=32`), against Postgres 16 in Docker (local
volume) with 2,005 records in the target entity. Generator: [`oha`
v1.15.0](https://github.com/hatoo/oha) on the same host (32 cores, WSL2).

## Results (30s steady-state per scenario)

| Scenario | Concurrency | Throughput | p50 | p99 | Slowest | Non-2xx |
|---|---|---|---|---|---|---|
| `GET /health` (DB round-trip) | 64 | ~13,800 rps | 4.0 ms | 13.6 ms | 47 ms | 0 |
| `GET /api/data/smokecustomer?page_size=20` (auth + RLS predicate + count + page) | 64 | ~1,415 rps | 41 ms | 140 ms | 188 ms | 0 |
| `POST /api/graphql` (`{ smokecustomers(first:20){name} }`) | 32 | ~1,440 rps | 20 ms | 79 ms | 166 ms | 0 |
| `POST /api/auth/login` (Argon2id verify) | 16 | ~172 rps | 79 ms | 201 ms | 272 ms | 0 |

Post-run state: server healthy, **zero 5xx / panics / error-log lines** across
~505k requests, RSS ~412 MB (includes the 32-connection pool + metadata
cache), database connections drained back to idle.

## Reading the numbers

- **Login throughput is low by design** — Argon2id costs ~70 ms of deliberate
  CPU per verification (brute-force resistance, §3). A login burst is CPU-bound
  on that cost; ~170 rps per node is the expected envelope, not a bottleneck to
  "fix". Everything behind the bearer token skips it.
- **The dynamic read path (~1.4k rps/node at p99 ≈ 140 ms under c=64) is the
  number to watch at scale.** Each list request runs the RLS visibility
  predicate (owner/team/shares) + a count + a page query as `mda_app`. For
  scale-out: nodes are stateless (cache invalidation is LISTEN/NOTIFY-based),
  so read throughput scales ~linearly behind a load balancer; Postgres
  connection ceiling becomes the next limit (pool default is 10 — raise
  `MDA_DB_MAX_CONNECTIONS` per node deliberately).
- **Health at ~14k rps** means liveness probes will never be the constraint.

## Reproduce

```bash
cargo build --release --bin mda-server
# boot as the production role (see docs/HARDENING.md), then:
oha -z 30s -c 64 -H "authorization: Bearer $TOKEN" \
    "http://localhost:8080/api/data/<entity>?page_size=20"
```

## Not yet covered (tracked)

- Sustained multi-hour soak (memory-growth trend beyond this 4½-minute run)
- Write-path saturation (rules + workflow + outbox fan-in per insert) — the
  single-record write pipeline is exercised functionally by the integration
  suites, not under load
- Report export concurrency (XLSX/PDF generation is CPU-bound per request)
- k6-style staged ramp to find the actual saturation point per endpoint
