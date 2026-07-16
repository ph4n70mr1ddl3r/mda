-- Phase 10: attachments & blob storage (PLAN §4.7 / §5.14). Bytes live in a
-- BlobStore (local FS here; S3 impl is a follow-up); only metadata is in Postgres.

CREATE TABLE IF NOT EXISTS sys_blob (
    id           UUID        PRIMARY KEY,
    tenant_id    UUID        NOT NULL,
    storage      TEXT        NOT NULL,    -- local | s3 | ...
    storage_key  TEXT        NOT NULL,
    filename     TEXT,
    mime         TEXT,
    size         BIGINT      NOT NULL DEFAULT 0,
    checksum     TEXT,
    owner_id     UUID,
    scan_status  TEXT        NOT NULL DEFAULT 'pending', -- pending | clean | quarantined
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS sys_blob_owner_idx ON sys_blob (tenant_id, owner_id);

CREATE TABLE IF NOT EXISTS sys_blob_ref (
    blob_id   UUID        NOT NULL REFERENCES sys_blob(id) ON DELETE CASCADE,
    tenant_id UUID        NOT NULL,
    entity    TEXT        NOT NULL,
    record_id UUID        NOT NULL,
    field     TEXT        NOT NULL,
    PRIMARY KEY (blob_id, entity, record_id, field)
);
