-- Phase 3: security principals + object/field permissions + OWD + audit log
-- (PLAN §4.5, §5.11). Record-level sharing rules / role hierarchy / materialized
-- sec_record_share arrive in Phase 6 (ADR-0013); Postgres RLS is a hardening
-- follow-up (§5.4). App-layer tenant isolation (every query bound to the
-- auth-derived tenant) is the primary isolation mechanism.

CREATE SCHEMA IF NOT EXISTS sec;

CREATE TABLE IF NOT EXISTS sec.sec_team (
    id          UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   UUID        NOT NULL,
    name        TEXT        NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, name)
);

CREATE TABLE IF NOT EXISTS sec.sec_user (
    id            UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id     UUID        NOT NULL,
    team_id       UUID        REFERENCES sec.sec_team(id) ON DELETE SET NULL,
    email         TEXT        NOT NULL,
    name          TEXT,
    password_hash TEXT        NOT NULL,
    active        BOOLEAN     NOT NULL DEFAULT TRUE,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, email)
);
CREATE INDEX IF NOT EXISTS sec_user_tenant_idx ON sec.sec_user (tenant_id);

CREATE TABLE IF NOT EXISTS sec.sec_role (
    id          UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   UUID        NOT NULL,
    name        TEXT        NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, name)
);

-- user <-> role (Phase 3: unscoped; scoped roles arrive with sharing in Phase 6)
CREATE TABLE IF NOT EXISTS sec.sec_role_assignment (
    user_id     UUID        NOT NULL REFERENCES sec.sec_user(id) ON DELETE CASCADE,
    role_id     UUID        NOT NULL REFERENCES sec.sec_role(id) ON DELETE CASCADE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, role_id)
);

-- object-level CRUD: entity is an entity name or '*' (wildcard); verb in
-- read|create|update|delete|'*'.
CREATE TABLE IF NOT EXISTS sec.sec_permission (
    role_id     UUID        NOT NULL REFERENCES sec.sec_role(id) ON DELETE CASCADE,
    entity      TEXT        NOT NULL,
    verb        TEXT        NOT NULL,
    PRIMARY KEY (role_id, entity, verb)
);

-- field-level: access in none|read|write. Absence of a row => full (write),
-- i.e. FLS is opt-in restriction in Phase 3.
CREATE TABLE IF NOT EXISTS sec.sec_field_permission (
    role_id     UUID        NOT NULL REFERENCES sec.sec_role(id) ON DELETE CASCADE,
    entity      TEXT        NOT NULL,
    field       TEXT        NOT NULL,
    access      TEXT        NOT NULL,   -- none | read | write
    PRIMARY KEY (role_id, entity, field)
);

-- org-wide default per entity: private | team | public_read | public_read_write
CREATE TABLE IF NOT EXISTS sec.sec_owd (
    tenant_id   UUID        NOT NULL,
    entity      TEXT        NOT NULL,
    default_access TEXT        NOT NULL DEFAULT 'private',
    PRIMARY KEY (tenant_id, entity)
);

-- audit log: every write, who/when/what, before/after (PLAN §4.7).
-- Time-partitioning + retention arrive with §5.15; a plain table for Phase 3.
CREATE TABLE IF NOT EXISTS sys_audit_log (
    id          UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id   UUID        NOT NULL,
    actor_id    UUID,
    entity      TEXT        NOT NULL,
    record_id   UUID        NOT NULL,
    op          TEXT        NOT NULL,   -- create | update | delete
    before      JSONB,
    after       JSONB,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS sys_audit_log_tenant_entity_record_idx
    ON sys_audit_log (tenant_id, entity, record_id, created_at DESC);
