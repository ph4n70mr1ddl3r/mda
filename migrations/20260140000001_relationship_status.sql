-- Two-phase destructive ops cover relationships too (PLAN §5.8: "drop field,
-- drop entity, drop relationship | Phase 1 retire ... set status = retired on
-- the md_* definition (md_entity / md_field / md_relationship)"). md_entity
-- and md_field already carry status; md_relationship did not — a removed
-- relationship was hard-dropped from the metadata (leaving its hoisted FK
-- column orphaned in biz.*) and could never be two-phase retired or un-retired.
ALTER TABLE meta.md_relationship
    ADD COLUMN IF NOT EXISTS status TEXT NOT NULL DEFAULT 'active';
CREATE INDEX IF NOT EXISTS md_relationship_status_idx
    ON meta.md_relationship (tenant_id, status);

-- Explicit many-to-many join-table name override (§5.7): NULL = the
-- deterministic `<smaller>_<larger>` name computed at publish time.
ALTER TABLE meta.md_relationship
    ADD COLUMN IF NOT EXISTS join_table_name TEXT;
