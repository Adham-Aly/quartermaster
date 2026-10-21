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

-- Memory is the working database from now on, and state.db mirrors only the kept rows (`qm-store`
-- `kept.rs`). That changes four constraints, and adds the block's watermarks.
--
-- A closed task, its turns and its events may leave memory while a row that names them stays, so
-- the references that can dangle become nullable, and the ones that can't be left out of the
-- schema are dropped. Rebuilding a table keeps its rows and its references, which name it by
-- table, so the deferred foreign-key check at commit sees the same data.

PRAGMA defer_foreign_keys = ON;

-- A standing instruction and a secondmate request name the message and turn that made them. Those
-- turns are not kept, so the references are optional.
CREATE TABLE standing_instructions_new (
    id              INTEGER PRIMARY KEY,
    text            TEXT    NOT NULL,
    message_id      INTEGER REFERENCES messages (id),
    turn_id         INTEGER REFERENCES turns (id),
    created_at      INTEGER NOT NULL,
    dropped_at      INTEGER,
    dropped_turn_id INTEGER REFERENCES turns (id),
    seq             INTEGER NOT NULL DEFAULT 0,
    CHECK ((dropped_at IS NULL) = (dropped_turn_id IS NULL))
) STRICT;
INSERT INTO standing_instructions_new (id, text, message_id, turn_id, created_at, dropped_at,
                                       dropped_turn_id, seq)
    SELECT id, text, message_id, turn_id, created_at, dropped_at, dropped_turn_id, seq
    FROM standing_instructions;
DROP TABLE standing_instructions;
ALTER TABLE standing_instructions_new RENAME TO standing_instructions;
CREATE UNIQUE INDEX standing_instructions_seq ON standing_instructions (seq);

CREATE TABLE secondmate_requests_new (
    id            INTEGER PRIMARY KEY,
    secondmate_id INTEGER NOT NULL REFERENCES secondmates (id),
    turn_id       INTEGER REFERENCES turns (id),
    request       TEXT    NOT NULL,
    user_words    TEXT    NOT NULL DEFAULT '',
    instructions  TEXT    NOT NULL DEFAULT '[]' CHECK (json_valid(instructions)),
    base_branch   TEXT,
    overrides     TEXT    NOT NULL DEFAULT '{}' CHECK (json_valid(overrides)),
    status        TEXT    NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'answered', 'closed')),
    created_at    INTEGER NOT NULL,
    seq           INTEGER NOT NULL DEFAULT 0
) STRICT;
INSERT INTO secondmate_requests_new (id, secondmate_id, turn_id, request, user_words, instructions,
                                     base_branch, overrides, status, created_at, seq)
    SELECT id, secondmate_id, turn_id, request, user_words, instructions, base_branch, overrides,
           status, created_at, seq
    FROM secondmate_requests;
DROP TABLE secondmate_requests;
ALTER TABLE secondmate_requests_new RENAME TO secondmate_requests;
CREATE UNIQUE INDEX secondmate_requests_seq ON secondmate_requests (seq);

-- A task names the task it builds on, the one it is a sub-task of and the one it retries. Those
-- may be forgotten while this task stays open, so the references are not enforced; the retry
-- trigger below checks what it can, and only when the original is still here.
CREATE TABLE tasks_new (
    id                    INTEGER PRIMARY KEY,
    project_id            INTEGER NOT NULL REFERENCES projects (id),
    supervisor_id         INTEGER NOT NULL REFERENCES agents (id),
    kind                  TEXT    NOT NULL CHECK (kind IN ('build', 'fix', 'plan', 'research', 'lookup', 'teach', 'review', 'unclear', 'reproduce')),
    title                 TEXT    NOT NULL,
    goal                  TEXT    NOT NULL,
    user_words            TEXT    NOT NULL DEFAULT '',
    user_words_message_id INTEGER REFERENCES messages (id),
    instructions          TEXT    NOT NULL DEFAULT '[]' CHECK (json_valid(instructions)),
    constraints           TEXT    NOT NULL DEFAULT '[]' CHECK (json_valid(constraints)),
    tests_required        INTEGER NOT NULL DEFAULT 0 CHECK (tests_required IN (0, 1)),
    builds_on             INTEGER,
    parent_task_id        INTEGER,
    retry_of              INTEGER,
    later_steps           TEXT    NOT NULL DEFAULT '[]' CHECK (json_valid(later_steps)),
    secondmate_request_id INTEGER REFERENCES secondmate_requests (id),
    delivery_mode         TEXT    NOT NULL CHECK (delivery_mode IN ('direct_pr', 'branch_only', 'build_in_place', 'folder')),
    verification_level    TEXT    CHECK (verification_level IN ('quick', 'standard', 'reviewed')),
    verification_source   TEXT    CHECK (verification_source IN ('user', 'project', 'default')),
    base_branch           TEXT,
    base_ref              TEXT,
    base_commit           TEXT,
    branch                TEXT,
    port_base             INTEGER,
    status                TEXT    NOT NULL DEFAULT 'queued' CHECK (status IN ('queued', 'preparing', 'running', 'waiting', 'held', 'stuck', 'reported', 'in_review', 'settling', 'landed', 'closed', 'failed', 'stopped', 'discarded')),
    outcome               TEXT    CHECK (outcome IN ('landed', 'answered', 'failed', 'stopped', 'discarded')),
    waiting_on            TEXT    CHECK (waiting_on IS NULL OR json_valid(waiting_on)),
    pr_number             INTEGER,
    head_commit           TEXT,
    repair_attempts       INTEGER NOT NULL DEFAULT 0 CHECK (repair_attempts >= 0),
    review_rounds         INTEGER NOT NULL DEFAULT 0 CHECK (review_rounds >= 0),
    escalations           INTEGER NOT NULL DEFAULT 0 CHECK (escalations >= 0),
    flake_rerun_used      INTEGER NOT NULL DEFAULT 0 CHECK (flake_rerun_used IN (0, 1)),
    created_at            INTEGER NOT NULL,
    started_at            INTEGER,
    reported_at           INTEGER,
    closed_at             INTEGER,
    pushed_head           TEXT,
    checks_state          TEXT CHECK (checks_state IS NULL OR json_valid(checks_state)),
    landed_commit         TEXT,
    landed_into           TEXT,
    repair_pending        TEXT    NOT NULL DEFAULT '{}' CHECK (json_valid(repair_pending)),
    repair_held           INTEGER NOT NULL DEFAULT 0 CHECK (repair_held IN (0, 1)),
    review_passed_at      INTEGER,
    validation_pending    INTEGER NOT NULL DEFAULT 0 CHECK (validation_pending IN (0, 1)),
    seq                   INTEGER NOT NULL DEFAULT 0,
    CHECK ((verification_level IS NULL) = (verification_source IS NULL)),
    CHECK ((status = 'closed') = (closed_at IS NOT NULL)),
    CHECK ((status = 'closed') = (outcome IS NOT NULL)),
    CHECK ((delivery_mode = 'folder') = (branch IS NULL)),
    CHECK (delivery_mode <> 'folder' OR (base_branch IS NULL AND base_ref IS NULL AND base_commit IS NULL))
) STRICT;
INSERT INTO tasks_new (id, project_id, supervisor_id, kind, title, goal, user_words,
                       user_words_message_id, instructions, constraints, tests_required, builds_on,
                       parent_task_id, retry_of, later_steps, secondmate_request_id, delivery_mode,
                       verification_level, verification_source, base_branch, base_ref, base_commit,
                       branch, port_base, status, outcome, waiting_on, pr_number, head_commit,
                       repair_attempts, review_rounds, escalations, flake_rerun_used, created_at,
                       started_at, reported_at, closed_at, pushed_head, checks_state, landed_commit,
                       landed_into, repair_pending, repair_held, review_passed_at,
                       validation_pending, seq)
    SELECT id, project_id, supervisor_id, kind, title, goal, user_words, user_words_message_id,
           instructions, constraints, tests_required, builds_on, parent_task_id, retry_of,
           later_steps, secondmate_request_id, delivery_mode, verification_level,
           verification_source, base_branch, base_ref, base_commit, branch, port_base, status,
           outcome, waiting_on, pr_number, head_commit, repair_attempts, review_rounds,
           escalations, flake_rerun_used, created_at, started_at, reported_at, closed_at,
           pushed_head, checks_state, landed_commit, landed_into, repair_pending, repair_held,
           review_passed_at, validation_pending, seq
    FROM tasks;
DROP TABLE tasks;
ALTER TABLE tasks_new RENAME TO tasks;
CREATE UNIQUE INDEX tasks_branch ON tasks (branch);
CREATE INDEX tasks_open ON tasks (project_id, status) WHERE status <> 'closed';
CREATE INDEX tasks_supervisor ON tasks (supervisor_id);
CREATE INDEX tasks_retry_of ON tasks (retry_of) WHERE retry_of IS NOT NULL;
CREATE UNIQUE INDEX tasks_port_open ON tasks (port_base)
    WHERE port_base IS NOT NULL AND status <> 'closed';
CREATE UNIQUE INDEX tasks_seq ON tasks (seq);
CREATE TRIGGER tasks_retry_carries_escalations BEFORE INSERT ON tasks
WHEN NEW.retry_of IS NOT NULL
    AND EXISTS (SELECT 1 FROM tasks WHERE id = NEW.retry_of)
    AND NEW.escalations IS NOT (SELECT escalations + 1 FROM tasks WHERE id = NEW.retry_of)
BEGIN
    SELECT RAISE(ABORT, 'a retry has one more escalation than the task it retries');
END;
CREATE TRIGGER tasks_retry_of_is_fixed BEFORE UPDATE OF retry_of ON tasks
WHEN NEW.retry_of IS NOT OLD.retry_of
BEGIN
    SELECT RAISE(ABORT, 'retry_of is set when the retry is created');
END;

-- Forgetting a closed task's events is a deliberate deletion (`qm-store` `history.rs`). Events
-- stay append-only otherwise: nothing updates them, and the disk mirror removes an event only when
-- it is no longer kept.
DROP TRIGGER events_no_delete;

-- The block's watermark per orchestrator: the last turn it started and the last event it saw, as
-- a turn ID and an event seq (`seq` orders, the ID names the row). Rolled back when a turn ends
-- with no wakes.
CREATE TABLE block_watermarks (
    agent_id      INTEGER PRIMARY KEY REFERENCES agents (id),
    turn_id       INTEGER,
    event_id      INTEGER,
    event_seq     INTEGER NOT NULL DEFAULT 0,
    prev_turn_id   INTEGER,
    prev_event_id  INTEGER,
    prev_event_seq INTEGER
) STRICT, WITHOUT ROWID;
