-- 0008: the plain-folder delivery mode. A folder task works directly in its folder: no branch, no
-- base, no commits. Its CHECKs need `folder`, and its branch can be NULL, which SQLite can't alter
-- in place, so `projects` and `tasks` are rebuilt. Foreign keys point at them by name, and every
-- row keeps its id, so the deferred check passes at commit (`foreign_key_check` runs after).

PRAGMA defer_foreign_keys = ON;

CREATE TABLE projects_new (
    id                 INTEGER PRIMARY KEY,
    name               TEXT    NOT NULL,
    path               TEXT    NOT NULL,
    origin             TEXT    NOT NULL CHECK (origin IN ('linked', 'cloned', 'created', 'workspace')),
    default_branch     TEXT    NOT NULL,
    github_remote      TEXT,
    delivery_mode      TEXT    NOT NULL CHECK (delivery_mode IN ('direct_pr', 'branch_only', 'build_in_place', 'folder')),
    autonomy           TEXT    NOT NULL DEFAULT 'off' CHECK (autonomy IN ('off', 'on')),
    verification_level TEXT    NOT NULL DEFAULT 'standard' CHECK (verification_level IN ('quick', 'standard', 'reviewed')),
    merge_method       TEXT    NOT NULL CHECK (merge_method IN ('merge', 'squash', 'rebase')),
    files_to_copy      TEXT    NOT NULL DEFAULT '[]' CHECK (json_valid(files_to_copy) AND json_type(files_to_copy) = 'array'),
    setup_command      TEXT,
    created_at         INTEGER NOT NULL,
    removed_at         INTEGER
) STRICT;

INSERT INTO projects_new (id, name, path, origin, default_branch, github_remote, delivery_mode,
                          autonomy, verification_level, merge_method, files_to_copy, setup_command,
                          created_at, removed_at)
    SELECT id, name, path, origin, default_branch, github_remote, delivery_mode,
           autonomy, verification_level, merge_method, files_to_copy, setup_command,
           created_at, removed_at FROM projects;

DROP TABLE projects;
ALTER TABLE projects_new RENAME TO projects;

CREATE UNIQUE INDEX projects_name_active ON projects (name) WHERE removed_at IS NULL;
CREATE UNIQUE INDEX projects_path_active ON projects (path) WHERE removed_at IS NULL;
CREATE UNIQUE INDEX projects_one_workspace ON projects (origin) WHERE origin = 'workspace';

-- The built-in personal workspace is a plain folder from now on. Its folder and its git repository
-- stay where they are, and the tasks already in it keep their own recorded modes.
UPDATE projects SET delivery_mode = 'folder' WHERE origin = 'workspace';

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
    builds_on             INTEGER REFERENCES tasks (id),
    parent_task_id        INTEGER REFERENCES tasks (id),
    -- A stronger-model retry is a new task that names the one it retries.
    retry_of              INTEGER REFERENCES tasks (id),
    later_steps           TEXT    NOT NULL DEFAULT '[]' CHECK (json_valid(later_steps)),
    secondmate_request_id INTEGER REFERENCES secondmate_requests (id),
    -- The mode is recorded when the task is created; a task keeps it for its whole life.
    delivery_mode         TEXT    NOT NULL CHECK (delivery_mode IN ('direct_pr', 'branch_only', 'build_in_place', 'folder')),
    verification_level    TEXT    CHECK (verification_level IN ('quick', 'standard', 'reviewed')),
    verification_source   TEXT    CHECK (verification_source IN ('user', 'project', 'default')),
    base_branch           TEXT,
    base_ref              TEXT,
    base_commit           TEXT,
    -- NULL for a folder task, which has no branch.
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
    -- A level and where it came from go together; answer-only, review and build-in-place tasks have
    -- neither.
    CHECK ((verification_level IS NULL) = (verification_source IS NULL)),
    CHECK ((status = 'closed') = (closed_at IS NOT NULL)),
    CHECK ((status = 'closed') = (outcome IS NOT NULL)),
    -- A folder task has no branch and records no base; every other task has a branch.
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
                       landed_into, repair_pending, repair_held, review_passed_at, validation_pending)
    SELECT id, project_id, supervisor_id, kind, title, goal, user_words,
           user_words_message_id, instructions, constraints, tests_required, builds_on,
           parent_task_id, retry_of, later_steps, secondmate_request_id, delivery_mode,
           verification_level, verification_source, base_branch, base_ref, base_commit,
           branch, port_base, status, outcome, waiting_on, pr_number, head_commit,
           repair_attempts, review_rounds, escalations, flake_rerun_used, created_at,
           started_at, reported_at, closed_at, pushed_head, checks_state, landed_commit,
           landed_into, repair_pending, repair_held, review_passed_at, validation_pending FROM tasks;

DROP TABLE tasks;
ALTER TABLE tasks_new RENAME TO tasks;

CREATE UNIQUE INDEX tasks_branch ON tasks (branch);
CREATE INDEX tasks_open ON tasks (project_id, status) WHERE status <> 'closed';
CREATE INDEX tasks_supervisor ON tasks (supervisor_id);
CREATE INDEX tasks_retry_of ON tasks (retry_of) WHERE retry_of IS NOT NULL;
CREATE UNIQUE INDEX tasks_port_open ON tasks (port_base)
    WHERE port_base IS NOT NULL AND status <> 'closed';
-- A retry carries its escalations forward: one more than the task it retries, so "escalated twice"
-- holds across the whole chain. A backstop to the code that creates it.
CREATE TRIGGER tasks_retry_carries_escalations BEFORE INSERT ON tasks
WHEN NEW.retry_of IS NOT NULL
    AND NEW.escalations IS NOT (SELECT escalations + 1 FROM tasks WHERE id = NEW.retry_of)
BEGIN
    SELECT RAISE(ABORT, 'a retry has one more escalation than the task it retries');
END;
CREATE TRIGGER tasks_retry_of_is_fixed BEFORE UPDATE OF retry_of ON tasks
WHEN NEW.retry_of IS NOT OLD.retry_of
BEGIN
    SELECT RAISE(ABORT, 'retry_of is set when the retry is created');
END;
