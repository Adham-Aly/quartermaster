-- Deferred starts allocate no task until their trigger fires. The full request and frozen
-- caller context are durable; references are seq-pinned in payload, never rebound by ID.
CREATE TABLE deferred_starts (
    id INTEGER PRIMARY KEY,
    seq INTEGER NOT NULL UNIQUE,
    owner_agent_id INTEGER NOT NULL,
    owner_seq INTEGER NOT NULL,
    trigger_json TEXT NOT NULL CHECK (json_valid(trigger_json)),
    payload TEXT NOT NULL CHECK (json_valid(payload)),
    status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'started', 'cancelled', 'failed')),
    task_id INTEGER,
    note TEXT,
    created_at INTEGER NOT NULL,
    finished_at INTEGER,
    CHECK ((status = 'started') = (task_id IS NOT NULL))
) STRICT;
CREATE INDEX deferred_starts_pending ON deferred_starts(seq) WHERE status = 'pending';
