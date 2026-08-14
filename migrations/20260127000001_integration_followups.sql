-- Integration follow-ups (PLAN §5.22 deferrals, now implemented):
--   * per-flow `running_user_id` — the hub writes under a scoped principal
--     instead of a blanket system superuser (scoped AuthZ for the hub's writes);
--   * `config` JSONB — flow-level configuration, currently carrying the
--     `sor_fields` list used by the `field_level_sor` conflict policy (the set of
--     canonical fields this external system is the authoritative source for).
--
-- Both columns are nullable/defaulted so existing flows keep their previous
-- behaviour (system principal + last_write_wins). int.flow is already RLS-gated
-- by tenant_id; the new columns inherit that policy unchanged.

ALTER TABLE int.flow ADD COLUMN IF NOT EXISTS running_user_id UUID;
ALTER TABLE int.flow
    ADD COLUMN IF NOT EXISTS config JSONB NOT NULL DEFAULT '{}'::jsonb;
