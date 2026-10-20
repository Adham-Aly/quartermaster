-- What a worker's session shows: append-only, one row per line.
CREATE TABLE session_lines (
    id         INTEGER PRIMARY KEY,
    agent_id   INTEGER NOT NULL REFERENCES agents (id),
    task_id    INTEGER REFERENCES tasks (id),
    turn_id    INTEGER REFERENCES turns (id),
    kind       TEXT    NOT NULL CHECK (kind IN ('supervisor', 'app', 'you', 'agent', 'tool')),
    text       TEXT    NOT NULL,
    detail     TEXT,
    message_id INTEGER REFERENCES messages (id),
    created_at INTEGER NOT NULL
) STRICT;
CREATE INDEX session_lines_agent ON session_lines (agent_id, id);
CREATE INDEX session_lines_message ON session_lines (message_id) WHERE message_id IS NOT NULL;
CREATE TRIGGER session_lines_no_update BEFORE UPDATE ON session_lines
BEGIN
    SELECT RAISE(ABORT, 'session lines are append-only');
END;
