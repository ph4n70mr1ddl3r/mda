-- Team hierarchy (ADR-0013 refinement): a parent/sub-team visibility tree.
--
-- Flat team-OWD is already honored (`owd_visible`): an entity whose OWD is
-- `team` admits the owner's teammates for read. This slice adds the *hierarchy*:
-- a member of a parent (ancestor) team may read records owned by members of any
-- descendant team — the "manager sees below" rule. Write stays owner-only
-- (mirrors `PublicRead`).
--
-- `parent_id` is a self-FK on `sec_team`. NULL => the team is a root. Cycles are
-- prevented in app code (and would be harmless to the recursive descent: a
-- cycle just re-visits teams already in the set). The record-visibility
-- predicate walks the tree DOWNWARD from the viewer's team; recipient
-- resolution walks UPWARD from the owner's team.

ALTER TABLE sec.sec_team ADD COLUMN IF NOT EXISTS parent_id UUID
    REFERENCES sec.sec_team(id) ON DELETE SET NULL;

CREATE INDEX IF NOT EXISTS sec_team_parent_idx
    ON sec.sec_team (tenant_id, parent_id);
