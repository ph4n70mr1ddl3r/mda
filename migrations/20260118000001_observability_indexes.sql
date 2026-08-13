-- Indexes for the §14 surfaced capabilities: record/field history + as-of
-- (read by tenant+entity+record_id, ordered by time) and the tenant
-- observability console (tenant-scoped scans of sys_event_log / sys_outbox).
--
-- The existing covering indexes were entity/record-grained and adequate, but
-- these tenant-first indexes make the new admin/timeline scans index-only on
-- the common filter shape. Idempotent (IF NOT EXISTS).

-- timeline + as-of: already covered by sys_audit_log_tenant_entity_record_idx
-- (tenant_id, entity, record_id, created_at DESC) — no duplicate needed.

-- observability: events by tenant (optionally type/entity), newest-first.
CREATE INDEX IF NOT EXISTS sys_event_log_tenant_type_seq_idx
    ON sys_event_log (tenant_id, type, seq DESC);

-- observability: outbox breakdown + outstanding-by-age per tenant.
CREATE INDEX IF NOT EXISTS sys_outbox_tenant_status_created_idx
    ON sys_outbox (tenant_id, status, created_at);
