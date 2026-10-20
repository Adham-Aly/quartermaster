-- 0002: what the crew needs on top of 0001.

-- Each open task holds its own block of ports; a closed task's block is free again.
CREATE UNIQUE INDEX tasks_port_open ON tasks (port_base)
    WHERE port_base IS NOT NULL AND status <> 'closed';
-- Running jobs, read for the watchdogs' signs of life.
CREATE INDEX jobs_running ON jobs (agent_id) WHERE exited_at IS NULL;
