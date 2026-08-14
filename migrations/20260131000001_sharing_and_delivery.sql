-- Closes the remaining ADR-0013 surface (criteria-based sharing rules +
-- role hierarchy) and scheduled-report delivery (PLAN Phase 7):
--
--  * sys_schedule.config — generic per-schedule options. For kind=report it
--    carries delivery: {"notify": true} dispatches a `report.completed`
--    notification (in-app channel, §5.18) to the running user summarizing the
--    run; email channels follow the notification type's routing as usual.
--    (No schema for sec_share_rule / sec_role_hierarchy is needed — both tables
--    exist since 20260109; this migration only documents their activation.)
--
-- Sharing-rule enforcement notes (ADR-0013, implemented in mda-data):
--   rule_visible(U,R) honors a materialized sec_record_share row ONLY when
--   rs.rule_id IS NULL (manual share) or rs.epoch = the rule's current epoch
--   AND the rule is active — so bumping sec_share_rule.epoch instantly revokes
--   every share computed under the old epoch (revoke-safe invalidation).
--   Per-record recompute runs SYNCHRONOUSLY in the write transaction
--   (create/update/delete); admin rule edits bump the epoch and re-materialize
--   in bounded keyset batches (POST /api/admin/share-rules/:id/recompute).

ALTER TABLE sys_schedule
    ADD COLUMN IF NOT EXISTS config JSONB NOT NULL DEFAULT '{}'::jsonb;
