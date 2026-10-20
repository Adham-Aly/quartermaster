-- 0004: delivery and settling. Columns only; no row changes.

-- The commit the app last pushed for this task (pr_open, or a branch-only settle): the lease for
-- the next push and the commit the CI poller watches.
ALTER TABLE tasks ADD COLUMN pushed_head TEXT;
-- The pushed head's checks as last read:
-- {"head","final","verdict","failing":[...],"passed","total","reruns","at"}.
ALTER TABLE tasks ADD COLUMN checks_state TEXT CHECK (checks_state IS NULL OR json_valid(checks_state));
-- Where landed work went: the merge commit (or the delivered head), and the branch.
ALTER TABLE tasks ADD COLUMN landed_commit TEXT;
ALTER TABLE tasks ADD COLUMN landed_into TEXT;

-- A task's events by kind: the card facts, the settle audit trail and startup recovery read them.
CREATE INDEX events_task_kind ON events (task_id, kind, id) WHERE task_id IS NOT NULL;
