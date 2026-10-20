-- 0001: every table of the app's state, including processes, jobs, a secondmate's projects and
-- its routed requests.
--
-- Conventions:
--   * every table is STRICT; foreign keys are on for every connection;
--   * instants are INTEGER milliseconds since the Unix epoch, UTC, in columns named *_at;
--   * booleans are INTEGER 0/1 with a CHECK;
--   * enums are TEXT holding the wire names of qm_types::domain, with a CHECK listing them
--     (wakes.kind and held_actions.kind are the exceptions: they grow, so Rust validates them);
--   * JSON columns are TEXT with CHECK (json_valid(...));
--   * display IDs (T-0042, SM-02, C-0193, SI-5, SC-7) are the INTEGER PRIMARY KEY of their table.

CREATE TABLE projects (
    id                 INTEGER PRIMARY KEY,
    name               TEXT    NOT NULL,
    path               TEXT    NOT NULL,
    origin             TEXT    NOT NULL CHECK (origin IN ('linked', 'cloned', 'created', 'workspace')),
    default_branch     TEXT    NOT NULL,
    github_remote      TEXT,
    delivery_mode      TEXT    NOT NULL CHECK (delivery_mode IN ('direct_pr', 'branch_only', 'build_in_place')),
    autonomy           TEXT    NOT NULL DEFAULT 'off' CHECK (autonomy IN ('off', 'on')),
    verification_level TEXT    NOT NULL DEFAULT 'standard' CHECK (verification_level IN ('quick', 'standard', 'reviewed')),
    merge_method       TEXT    NOT NULL CHECK (merge_method IN ('merge', 'squash', 'rebase')),
    files_to_copy      TEXT    NOT NULL DEFAULT '[]' CHECK (json_valid(files_to_copy) AND json_type(files_to_copy) = 'array'),
    setup_command      TEXT,
    created_at         INTEGER NOT NULL,
    removed_at         INTEGER
) STRICT;
CREATE UNIQUE INDEX projects_name_active ON projects (name) WHERE removed_at IS NULL;
CREATE UNIQUE INDEX projects_path_active ON projects (path) WHERE removed_at IS NULL;
CREATE UNIQUE INDEX projects_one_workspace ON projects (origin) WHERE origin = 'workspace';

CREATE TABLE accounts (
    id             INTEGER PRIMARY KEY,
    provider       TEXT    NOT NULL CHECK (provider IN ('claude', 'chatgpt', 'copilot', 'xai', 'kimi', 'openrouter', 'gemini', 'anthropic', 'openai', 'custom', 'scripted')),
    label          TEXT    NOT NULL,
    kind           TEXT    NOT NULL CHECK (kind IN ('subscription', 'api_key')),
    engine         TEXT    NOT NULL CHECK (engine IN ('claude', 'provider', 'scripted')),
    paused         INTEGER NOT NULL DEFAULT 0 CHECK (paused IN (0, 1)),
    preferred      INTEGER NOT NULL DEFAULT 0 CHECK (preferred IN (0, 1)),
    usage          TEXT    CHECK (usage IS NULL OR json_valid(usage)),
    usage_read_at  INTEGER,
    credential_ref TEXT,
    base_url       TEXT,
    created_at     INTEGER NOT NULL,
    removed_at     INTEGER,
    -- The engine follows the account.
    CHECK ((provider = 'claude') = (engine = 'claude')),
    CHECK ((provider = 'scripted') = (engine = 'scripted'))
) STRICT;
CREATE UNIQUE INDEX accounts_label_active ON accounts (label) WHERE removed_at IS NULL;
CREATE UNIQUE INDEX accounts_one_preferred ON accounts (preferred) WHERE preferred = 1 AND removed_at IS NULL;

CREATE TABLE models (
    id            INTEGER PRIMARY KEY,
    account_id    INTEGER NOT NULL REFERENCES accounts (id),
    model         TEXT    NOT NULL,
    canonical     TEXT    NOT NULL,
    family        TEXT,
    display_name  TEXT,
    capabilities  TEXT    NOT NULL DEFAULT '{}' CHECK (json_valid(capabilities)),
    efforts       TEXT    NOT NULL DEFAULT '[]' CHECK (json_valid(efforts)),
    first_seen_at INTEGER NOT NULL,
    is_new        INTEGER NOT NULL DEFAULT 1 CHECK (is_new IN (0, 1)),
    gone_at       INTEGER,
    UNIQUE (account_id, model)
) STRICT;

CREATE TABLE placements (
    id              INTEGER PRIMARY KEY,
    canonical       TEXT    NOT NULL,
    tier            TEXT    NOT NULL CHECK (tier IN ('low', 'medium', 'high', 'premium')),
    effort_override TEXT    CHECK (effort_override IN ('low', 'medium', 'high', 'xhigh', 'max')),
    created_at      INTEGER NOT NULL,
    UNIQUE (canonical, tier)
) STRICT;

CREATE TABLE secondmates (
    id                   INTEGER PRIMARY KEY,
    name                 TEXT    NOT NULL,
    scope                TEXT    NOT NULL,
    standing_base_branch TEXT,
    created_at           INTEGER NOT NULL,
    retired_at           INTEGER
) STRICT;
CREATE UNIQUE INDEX secondmates_name_active ON secondmates (name) WHERE retired_at IS NULL;

CREATE TABLE secondmate_projects (
    secondmate_id INTEGER NOT NULL REFERENCES secondmates (id),
    project_id    INTEGER NOT NULL REFERENCES projects (id),
    PRIMARY KEY (secondmate_id, project_id)
) STRICT, WITHOUT ROWID;

CREATE TABLE agents (
    id                   INTEGER PRIMARY KEY,
    role                 TEXT    NOT NULL CHECK (role IN ('quartermaster', 'secondmate', 'crewmate')),
    supervisor_id        INTEGER REFERENCES agents (id),
    secondmate_id        INTEGER REFERENCES secondmates (id),
    task_id              INTEGER REFERENCES tasks (id),
    engine               TEXT    CHECK (engine IN ('claude', 'provider', 'scripted')),
    account_id           INTEGER REFERENCES accounts (id),
    model                TEXT,
    effort               TEXT    CHECK (effort IN ('low', 'medium', 'high', 'xhigh', 'max')),
    home                 TEXT    NOT NULL,
    session_ref          TEXT,
    status               TEXT    NOT NULL CHECK (status IN ('starting', 'idle', 'running', 'waiting', 'stopped', 'crashed', 'retired')),
    last_sign_of_life_at INTEGER,
    created_at           INTEGER NOT NULL,
    retired_at           INTEGER,
    CHECK ((role = 'quartermaster') = (supervisor_id IS NULL)),
    CHECK ((role = 'secondmate') = (secondmate_id IS NOT NULL)),
    CHECK ((role = 'crewmate') = (task_id IS NOT NULL))
) STRICT;
CREATE UNIQUE INDEX agents_one_quartermaster ON agents (role) WHERE role = 'quartermaster' AND retired_at IS NULL;
CREATE INDEX agents_task ON agents (task_id) WHERE task_id IS NOT NULL;

CREATE TABLE messages (
    id         INTEGER PRIMARY KEY,
    agent_id   INTEGER NOT NULL REFERENCES agents (id),
    source     TEXT    NOT NULL CHECK (source IN ('you', 'supervisor', 'app', 'agent')),
    kind       TEXT    NOT NULL CHECK (kind IN ('message', 'reply', 'question', 'acknowledgement', 'app_note')),
    text       TEXT    NOT NULL,
    options    TEXT    CHECK (options IS NULL OR json_valid(options)),
    replies_to INTEGER REFERENCES messages (id),
    turn_id    INTEGER REFERENCES turns (id),
    created_at INTEGER NOT NULL
) STRICT;
CREATE INDEX messages_agent ON messages (agent_id, id);

CREATE TABLE turns (
    id                 INTEGER PRIMARY KEY,
    agent_id           INTEGER NOT NULL REFERENCES agents (id),
    started_by         TEXT    NOT NULL CHECK (started_by IN ('user', 'app_event')),
    message_id         INTEGER REFERENCES messages (id),
    status             TEXT    NOT NULL DEFAULT 'running' CHECK (status IN ('running', 'completed', 'interrupted', 'failed')),
    outcome_reason     TEXT,
    effective_actions  TEXT    NOT NULL DEFAULT '[]' CHECK (json_valid(effective_actions)),
    retried            INTEGER NOT NULL DEFAULT 0 CHECK (retried IN (0, 1)),
    model              TEXT,
    model_calls        INTEGER NOT NULL DEFAULT 0,
    input_tokens       INTEGER NOT NULL DEFAULT 0,
    output_tokens      INTEGER NOT NULL DEFAULT 0,
    thinking_tokens    INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens  INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
    cost_usd           REAL,
    started_at         INTEGER NOT NULL,
    ended_at           INTEGER,
    -- A turn you started names your message.
    CHECK ((started_by = 'user') = (message_id IS NOT NULL)),
    CHECK ((status = 'running') = (ended_at IS NULL))
) STRICT;
-- Each agent runs one turn at a time.
CREATE UNIQUE INDEX turns_one_running ON turns (agent_id) WHERE status = 'running';

CREATE TABLE secondmate_requests (
    id            INTEGER PRIMARY KEY,
    secondmate_id INTEGER NOT NULL REFERENCES secondmates (id),
    turn_id       INTEGER NOT NULL REFERENCES turns (id),
    request       TEXT    NOT NULL,
    user_words    TEXT    NOT NULL DEFAULT '',
    instructions  TEXT    NOT NULL DEFAULT '[]' CHECK (json_valid(instructions)),
    base_branch   TEXT,
    overrides     TEXT    NOT NULL DEFAULT '{}' CHECK (json_valid(overrides)),
    status        TEXT    NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'answered', 'closed')),
    created_at    INTEGER NOT NULL
) STRICT;

CREATE TABLE tasks (
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
    delivery_mode         TEXT    NOT NULL CHECK (delivery_mode IN ('direct_pr', 'branch_only', 'build_in_place')),
    verification_level    TEXT    CHECK (verification_level IN ('quick', 'standard', 'reviewed')),
    verification_source   TEXT    CHECK (verification_source IN ('user', 'project', 'default')),
    base_branch           TEXT,
    base_ref              TEXT,
    base_commit           TEXT,
    branch                TEXT    NOT NULL,
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
    -- A level and where it came from go together; answer-only, review and build-in-place tasks have
    -- neither.
    CHECK ((verification_level IS NULL) = (verification_source IS NULL)),
    CHECK ((status = 'closed') = (closed_at IS NOT NULL)),
    CHECK ((status = 'closed') = (outcome IS NOT NULL))
) STRICT;
CREATE UNIQUE INDEX tasks_branch ON tasks (branch);
CREATE INDEX tasks_open ON tasks (project_id, status) WHERE status <> 'closed';
CREATE INDEX tasks_supervisor ON tasks (supervisor_id);
CREATE INDEX tasks_retry_of ON tasks (retry_of) WHERE retry_of IS NOT NULL;
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

CREATE TABLE routing_decisions (
    id          INTEGER PRIMARY KEY,
    task_id     INTEGER NOT NULL REFERENCES tasks (id),
    agent_id    INTEGER REFERENCES agents (id),
    facts       TEXT    NOT NULL CHECK (json_valid(facts)),
    rule        TEXT    NOT NULL,
    decided_by  TEXT    NOT NULL,
    tier_landed TEXT    NOT NULL CHECK (tier_landed IN ('low', 'medium', 'high', 'premium')),
    tier_used   TEXT    CHECK (tier_used IN ('low', 'medium', 'high', 'premium')),
    model       TEXT,
    account_id  INTEGER REFERENCES accounts (id),
    effort      TEXT    CHECK (effort IN ('low', 'medium', 'high', 'xhigh', 'max')),
    skipped     TEXT    NOT NULL DEFAULT '[]' CHECK (json_valid(skipped)),
    fallbacks   TEXT    NOT NULL DEFAULT '[]' CHECK (json_valid(fallbacks)),
    why         TEXT    NOT NULL,
    created_at  INTEGER NOT NULL,
    -- A decision either chose a model, account, effort and tier, or is waiting.
    CHECK ((model IS NULL) = (account_id IS NULL)),
    CHECK ((model IS NULL) = (effort IS NULL)),
    CHECK ((model IS NULL) = (tier_used IS NULL))
) STRICT;
CREATE INDEX routing_decisions_task ON routing_decisions (task_id);

CREATE TABLE task_links (
    blocker    INTEGER NOT NULL REFERENCES tasks (id),
    blocked    INTEGER NOT NULL REFERENCES tasks (id),
    created_at INTEGER NOT NULL,
    PRIMARY KEY (blocker, blocked),
    CHECK (blocker <> blocked)
) STRICT, WITHOUT ROWID;
-- Cycles are refused here too, as a backstop to the link_tasks rule.
CREATE TRIGGER task_links_no_cycles BEFORE INSERT ON task_links
WHEN EXISTS (
    WITH RECURSIVE reach (id) AS (
        SELECT NEW.blocked
        UNION
        SELECT l.blocked FROM task_links AS l JOIN reach AS r ON l.blocker = r.id
    )
    SELECT 1 FROM reach WHERE id = NEW.blocker
)
BEGIN
    SELECT RAISE(ABORT, 'task link would create a cycle');
END;

CREATE TABLE reports (
    id                 INTEGER PRIMARY KEY,
    task_id            INTEGER NOT NULL REFERENCES tasks (id),
    agent_id           INTEGER NOT NULL REFERENCES agents (id),
    source             TEXT    NOT NULL CHECK (source IN ('report_call', 'final_text')),
    outcome            TEXT    CHECK (outcome IN ('done', 'blocked', 'needs_decision')),
    headings           TEXT    NOT NULL DEFAULT '{}' CHECK (json_valid(headings)),
    answer             TEXT,
    reproduced         INTEGER CHECK (reproduced IN (0, 1)),
    diagnosis          INTEGER NOT NULL DEFAULT 0 CHECK (diagnosis IN (0, 1)),
    review_verdict     TEXT    CHECK (review_verdict IN ('pass', 'findings', 'blocked')),
    review_findings    TEXT    NOT NULL DEFAULT '[]' CHECK (json_valid(review_findings)),
    review_risk        TEXT    CHECK (review_risk IN ('low', 'medium', 'high')),
    review_risk_reason TEXT,
    blockers           TEXT    NOT NULL DEFAULT '[]' CHECK (json_valid(blockers)),
    head_commit        TEXT,
    final_text         TEXT,
    created_at         INTEGER NOT NULL,
    -- Only a report call is a clear report, with an outcome.
    CHECK ((source = 'report_call') = (outcome IS NOT NULL))
) STRICT;
CREATE INDEX reports_task ON reports (task_id, id);

CREATE TABLE held_actions (
    id         INTEGER PRIMARY KEY,
    -- qm_types::domain::HeldKind; no closed CHECK, because held kinds grow (validated by Rust).
    kind       TEXT    NOT NULL,
    payload    TEXT    NOT NULL CHECK (json_valid(payload) AND json_extract(payload, '$.kind') = kind),
    choices    TEXT    NOT NULL DEFAULT '[]' CHECK (json_valid(choices)),
    title      TEXT    NOT NULL,
    task_id    INTEGER REFERENCES tasks (id),
    asked_by   INTEGER REFERENCES agents (id),
    status     TEXT    NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'approved', 'rejected', 'answered', 'superseded')),
    answer     TEXT    CHECK (answer IS NULL OR json_valid(answer)),
    created_at INTEGER NOT NULL,
    decided_at INTEGER,
    CHECK ((status = 'pending') = (decided_at IS NULL))
) STRICT;
CREATE INDEX held_actions_pending ON held_actions (status) WHERE status = 'pending';

CREATE TABLE wakes (
    id                INTEGER PRIMARY KEY,
    target_agent_id   INTEGER NOT NULL REFERENCES agents (id),
    kind              TEXT    NOT NULL,
    payload           TEXT    NOT NULL DEFAULT '{}' CHECK (json_valid(payload)),
    dedupe_key        TEXT    NOT NULL,
    task_id           INTEGER REFERENCES tasks (id),
    created_at        INTEGER NOT NULL,
    delivered_turn_id INTEGER REFERENCES turns (id),
    delivered_at      INTEGER,
    consumed_at       INTEGER,
    -- The same news never wakes anyone twice.
    UNIQUE (target_agent_id, dedupe_key),
    CHECK ((delivered_turn_id IS NULL) = (delivered_at IS NULL)),
    CHECK (consumed_at IS NULL OR delivered_at IS NOT NULL)
) STRICT;
CREATE INDEX wakes_pending ON wakes (target_agent_id, id) WHERE consumed_at IS NULL;
CREATE INDEX wakes_delivered ON wakes (delivered_turn_id) WHERE consumed_at IS NULL;

CREATE TABLE events (
    id      INTEGER PRIMARY KEY,
    at      INTEGER NOT NULL,
    kind    TEXT    NOT NULL,
    agent_id INTEGER REFERENCES agents (id),
    task_id INTEGER REFERENCES tasks (id),
    turn_id INTEGER REFERENCES turns (id),
    summary TEXT    NOT NULL,
    payload TEXT    NOT NULL DEFAULT '{}' CHECK (json_valid(payload))
) STRICT;
CREATE INDEX events_at ON events (at);
CREATE TRIGGER events_no_update BEFORE UPDATE ON events
BEGIN
    SELECT RAISE(ABORT, 'events are append-only');
END;
CREATE TRIGGER events_no_delete BEFORE DELETE ON events
BEGIN
    SELECT RAISE(ABORT, 'events are append-only');
END;

CREATE TABLE parent_channel (
    id             INTEGER PRIMARY KEY,
    secondmate_id  INTEGER NOT NULL REFERENCES secondmates (id),
    correlation_id INTEGER NOT NULL REFERENCES secondmate_requests (id),
    facts          TEXT    NOT NULL CHECK (json_valid(facts)),
    judgment       TEXT    CHECK (judgment IS NULL OR json_valid(judgment)),
    created_at     INTEGER NOT NULL
) STRICT;

CREATE TABLE standing_instructions (
    id              INTEGER PRIMARY KEY,
    text            TEXT    NOT NULL,
    message_id      INTEGER NOT NULL REFERENCES messages (id),
    turn_id         INTEGER NOT NULL REFERENCES turns (id),
    created_at      INTEGER NOT NULL,
    dropped_at      INTEGER,
    dropped_turn_id INTEGER REFERENCES turns (id),
    CHECK ((dropped_at IS NULL) = (dropped_turn_id IS NULL))
) STRICT;

CREATE TABLE schedules (
    id              INTEGER PRIMARY KEY,
    owner_agent_id  INTEGER NOT NULL REFERENCES agents (id),
    due_at          INTEGER NOT NULL,
    tz              TEXT    NOT NULL,
    what            TEXT    NOT NULL,
    status          TEXT    NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'fired', 'cancelled')),
    created_turn_id INTEGER REFERENCES turns (id),
    fired_at        INTEGER,
    late            INTEGER NOT NULL DEFAULT 0 CHECK (late IN (0, 1)),
    created_at      INTEGER NOT NULL
) STRICT;
CREATE INDEX schedules_due ON schedules (due_at) WHERE status = 'pending';

CREATE TABLE jobs (
    id         INTEGER PRIMARY KEY,
    agent_id   INTEGER NOT NULL REFERENCES agents (id),
    task_id    INTEGER NOT NULL REFERENCES tasks (id),
    label      TEXT    NOT NULL,
    command    TEXT    NOT NULL,
    log_path   TEXT,
    started_at INTEGER NOT NULL,
    exited_at  INTEGER,
    exit_code  INTEGER,
    UNIQUE (agent_id, label)
) STRICT;

CREATE TABLE processes (
    id             INTEGER PRIMARY KEY,
    pgid           INTEGER NOT NULL,
    pid            INTEGER NOT NULL,
    -- For display only: the leftover check identifies a group by its leader's start time.
    program        TEXT    NOT NULL,
    owner          TEXT    NOT NULL CHECK (owner IN ('agent', 'job', 'setup', 'app')),
    agent_id       INTEGER REFERENCES agents (id),
    -- A job's process names its agent and job label; job_id is linked by whichever write comes
    -- second, the job's row (from the engine's JobStarted) or this one (from the supervisor).
    job_id         INTEGER REFERENCES jobs (id),
    task_id        INTEGER REFERENCES tasks (id),
    label          TEXT,
    -- The leader's start time as the OS reports it, not when the app asked for it.
    started_at     INTEGER NOT NULL,
    ended_at       INTEGER,
    exit_code      INTEGER,
    signal         INTEGER,
    stop_requested INTEGER NOT NULL DEFAULT 0 CHECK (stop_requested IN (0, 1)),
    CHECK (owner <> 'job' OR (agent_id IS NOT NULL AND label IS NOT NULL))
) STRICT;
CREATE INDEX processes_alive ON processes (pgid) WHERE ended_at IS NULL;
CREATE INDEX processes_unlinked_jobs ON processes (agent_id, label) WHERE owner = 'job' AND job_id IS NULL;
