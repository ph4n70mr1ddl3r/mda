# Phase 3 — Security & Auth (status & handoff)

**Status: complete & verified.** Implements PLAN §9 Phase 3: JWT auth, object
RBAC, field-level security, record-level ownership/OWD, and audit logging.
Deliverable met: *login; role-gated object + field + record access; tenant
isolation; full audit trail.*

## What was built

**`sec_*` + audit tables** (`migrations/20260103000001`): `sec_team`,
`sec_user`, `sec_role`, `sec_role_assignment`, `sec_permission`,
`sec_field_permission`, `sec_owd`, and `sys_audit_log`.

**`mda-security`** crate:
- Argon2id password hashing; JWT (HS256) access (15m) + refresh (7d).
- `Identity` (effective context): object perms (with `*` wildcards), field
  perms, `is_superuser`; `can(entity,verb)` + `field_access(entity,field)`.
- `load_identity` (roles → perms + field perms) and `resolve_owd`.

**AuthN** (`mda-api/auth.rs`): `POST /api/auth/login`, `/api/auth/refresh`,
`GET /api/auth/me`, and an `AuthUser` extractor (Bearer → `Identity`). **Tenant
isolation now comes from the verified token** — the client-controlled
`X-Tenant-Id` trust is removed; cross-tenant access is impossible.

**Object RBAC** (`sec_permission`): every `/api/data` verb checked → 403 on miss.
**Field-level security** (`sec_field_permission`): write-rejection on create/update
(403), read-projection on read/list (drops `none` fields). Absent rules ⇒ full
(opt-in restriction).

**Record-level** (ownership + OWD): a `RecordScope` injects an ownership/OWD
predicate into every CRUD query (never post-filtering). `sec_owd`
private/team/public_read/public_read_write; superuser bypass. `owner_id` set
from auth on create.

**Audit**: `sys_audit_log` row (before/after) on every create/update/delete.

**Bootstrap**: `mda-server` seeds an admin (`admin@mda.local` / `admin123` via
`MDA_BOOTSTRAP_PASSWORD`) with a superuser role on startup, idempotently.

## Verification (all green)

- `fmt` · `clippy -D warnings`
- unit: core(4) + draft(7) + ddl(1); schema integration(1); studio(3); data(4)
- `--test data` `auth_rbac_and_record_level_enforced`: no-token→401; no-perm
  user→403 (RBAC); read+create user→403 on delete (RBAC); a private record is
  **invisible (404) to a non-owner reader** (record-level); audit rows written.
- live `curl`: login→token→`/me`(superuser)→no-token 401→publish→create
  (`owner_id` = the admin).

## Phase-3 decisions (made autonomously, per the plan)

- **Tenant from the JWT**, not a header — that *is* the isolation; Postgres RLS
  (defense-in-depth) is a documented hardening follow-up.
- **Record-level = ownership + public OWD**; team-OWD (owner-team join) and
  criteria-based sharing / role hierarchy / materialized `sec_record_share`
  (ADR-0013) are explicitly **Phase 6**.
- **FLS defaults to full** when no rule exists (opt-in restriction).
- **Studio routes require a superuser role** for Phase 3 (a proper Studio role
  comes with Phase 8).
- Stateless JWT (no revocation list yet; `token_epoch` revocation is a follow-up).

## Deferred (visible, not lost)

- **Postgres RLS** (defense-in-depth) + per-transaction tenant GUC.
- **Phase 6 record security**: criteria sharing rules, role hierarchy,
  materialized `sec_record_share` with epoch invalidation (ADR-0013); team-OWD.
- **SSO/SAML/SCIM** (already deferred in §9); **token revocation list**.
- Effective-context cache via Redis (loaded per-request for now).

## How to try it

```bash
docker compose up -d postgres redis
DATABASE_URL=postgres://mda:mda@127.0.0.1:5433/mda?sslmode=disable \
MDA_JWT_SECRET=change-me cargo run

# login (bootstrap admin)
curl -X POST localhost:8080/api/auth/login -H "content-type: application/json" \
     -d '{"email":"admin@mda.local","password":"admin123"}'   # → access_token
TOK=...   # the access_token
curl localhost:8080/api/auth/me -H "Authorization: Bearer $TOK"
curl localhost:8080/api/data/Customer -H "Authorization: Bearer $TOK"
```
