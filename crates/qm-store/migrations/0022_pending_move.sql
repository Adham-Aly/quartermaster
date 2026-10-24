-- A crewmate's workplace can move while it runs (`set_task_project`): to another project, folder
-- or worktree. The supervisor's request is recorded here, as JSON, until the crewmate's runner
-- applies it between turns; the row's own columns keep describing where it works now. NULL when
-- no move waits.
ALTER TABLE tasks ADD COLUMN pending_move TEXT
    CHECK (pending_move IS NULL OR json_valid(pending_move));
