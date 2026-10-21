-- Creation order: every row gets a `seq`, the creation instant in microseconds made strictly
-- increasing (`qm-store` `seq.rs`). Existing rows keep their IDs, which were counted up and so sort
-- in creation order, so `seq` = `id` for them; every new row's seq is a microsecond instant, far above
-- any old ID.

ALTER TABLE projects ADD COLUMN seq INTEGER NOT NULL DEFAULT 0;
ALTER TABLE accounts ADD COLUMN seq INTEGER NOT NULL DEFAULT 0;
ALTER TABLE models ADD COLUMN seq INTEGER NOT NULL DEFAULT 0;
ALTER TABLE placements ADD COLUMN seq INTEGER NOT NULL DEFAULT 0;
ALTER TABLE secondmates ADD COLUMN seq INTEGER NOT NULL DEFAULT 0;
ALTER TABLE secondmate_projects ADD COLUMN seq INTEGER NOT NULL DEFAULT 0;
ALTER TABLE agents ADD COLUMN seq INTEGER NOT NULL DEFAULT 0;
ALTER TABLE messages ADD COLUMN seq INTEGER NOT NULL DEFAULT 0;
ALTER TABLE turns ADD COLUMN seq INTEGER NOT NULL DEFAULT 0;
ALTER TABLE secondmate_requests ADD COLUMN seq INTEGER NOT NULL DEFAULT 0;
ALTER TABLE tasks ADD COLUMN seq INTEGER NOT NULL DEFAULT 0;
ALTER TABLE routing_decisions ADD COLUMN seq INTEGER NOT NULL DEFAULT 0;
ALTER TABLE task_links ADD COLUMN seq INTEGER NOT NULL DEFAULT 0;
ALTER TABLE reports ADD COLUMN seq INTEGER NOT NULL DEFAULT 0;
ALTER TABLE held_actions ADD COLUMN seq INTEGER NOT NULL DEFAULT 0;
ALTER TABLE wakes ADD COLUMN seq INTEGER NOT NULL DEFAULT 0;
ALTER TABLE events ADD COLUMN seq INTEGER NOT NULL DEFAULT 0;
ALTER TABLE parent_channel ADD COLUMN seq INTEGER NOT NULL DEFAULT 0;
ALTER TABLE standing_instructions ADD COLUMN seq INTEGER NOT NULL DEFAULT 0;
ALTER TABLE schedules ADD COLUMN seq INTEGER NOT NULL DEFAULT 0;
ALTER TABLE jobs ADD COLUMN seq INTEGER NOT NULL DEFAULT 0;
ALTER TABLE processes ADD COLUMN seq INTEGER NOT NULL DEFAULT 0;
ALTER TABLE session_lines ADD COLUMN seq INTEGER NOT NULL DEFAULT 0;

UPDATE projects SET seq = id;
UPDATE accounts SET seq = id;
UPDATE models SET seq = id;
UPDATE placements SET seq = id;
UPDATE secondmates SET seq = id;
-- The two join tables have no ID and no rowid (`WITHOUT ROWID`): they number their rows in key order
-- (`task_links` in creation order). Their values are small, and new rows are microsecond instants.
WITH numbered AS (
    SELECT secondmate_id, project_id, ROW_NUMBER() OVER (ORDER BY secondmate_id, project_id) AS n
    FROM secondmate_projects
)
UPDATE secondmate_projects SET seq = (
    SELECT n FROM numbered
    WHERE numbered.secondmate_id = secondmate_projects.secondmate_id
      AND numbered.project_id = secondmate_projects.project_id
);
UPDATE agents SET seq = id;
UPDATE messages SET seq = id;
UPDATE turns SET seq = id;
UPDATE secondmate_requests SET seq = id;
UPDATE tasks SET seq = id;
UPDATE routing_decisions SET seq = id;
WITH numbered AS (
    SELECT blocker, blocked, ROW_NUMBER() OVER (ORDER BY created_at, blocker, blocked) AS n
    FROM task_links
)
UPDATE task_links SET seq = (
    SELECT n FROM numbered
    WHERE numbered.blocker = task_links.blocker AND numbered.blocked = task_links.blocked
);
UPDATE reports SET seq = id;
UPDATE held_actions SET seq = id;
UPDATE wakes SET seq = id;
-- `events` and `session_lines` are append-only: their update triggers are lifted for this backfill
-- only, and put back before the migration commits.
DROP TRIGGER events_no_update;
UPDATE events SET seq = id;
UPDATE parent_channel SET seq = id;
UPDATE standing_instructions SET seq = id;
UPDATE schedules SET seq = id;
UPDATE jobs SET seq = id;
UPDATE processes SET seq = id;
DROP TRIGGER session_lines_no_update;
UPDATE session_lines SET seq = id;
CREATE TRIGGER session_lines_no_update BEFORE UPDATE ON session_lines
BEGIN
    SELECT RAISE(ABORT, 'session lines are append-only');
END;

CREATE TRIGGER events_no_update BEFORE UPDATE ON events
BEGIN
    SELECT RAISE(ABORT, 'events are append-only');
END;

CREATE UNIQUE INDEX projects_seq ON projects (seq);
CREATE UNIQUE INDEX accounts_seq ON accounts (seq);
CREATE UNIQUE INDEX models_seq ON models (seq);
CREATE UNIQUE INDEX placements_seq ON placements (seq);
CREATE UNIQUE INDEX secondmates_seq ON secondmates (seq);
CREATE UNIQUE INDEX secondmate_projects_seq ON secondmate_projects (seq);
CREATE UNIQUE INDEX agents_seq ON agents (seq);
CREATE UNIQUE INDEX messages_seq ON messages (seq);
CREATE UNIQUE INDEX turns_seq ON turns (seq);
CREATE UNIQUE INDEX secondmate_requests_seq ON secondmate_requests (seq);
CREATE UNIQUE INDEX tasks_seq ON tasks (seq);
CREATE UNIQUE INDEX routing_decisions_seq ON routing_decisions (seq);
CREATE UNIQUE INDEX task_links_seq ON task_links (seq);
CREATE UNIQUE INDEX reports_seq ON reports (seq);
CREATE UNIQUE INDEX held_actions_seq ON held_actions (seq);
CREATE UNIQUE INDEX wakes_seq ON wakes (seq);
CREATE UNIQUE INDEX events_seq ON events (seq);
CREATE UNIQUE INDEX parent_channel_seq ON parent_channel (seq);
CREATE UNIQUE INDEX standing_instructions_seq ON standing_instructions (seq);
CREATE UNIQUE INDEX schedules_seq ON schedules (seq);
CREATE UNIQUE INDEX jobs_seq ON jobs (seq);
CREATE UNIQUE INDEX processes_seq ON processes (seq);
CREATE UNIQUE INDEX session_lines_seq ON session_lines (seq);
