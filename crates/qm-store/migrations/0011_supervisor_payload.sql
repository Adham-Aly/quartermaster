-- A `task.*` event's payload names its task's supervisor as `supervisor` (`tasks.rs` `event`), which
-- the context block and the kept outcomes read. Events written before that were never given it, so
-- an orchestrator's block lost its own tasks' lines and outcomes after an upgrade. Backfill it from
-- the task row, once. `events` is append-only: its update trigger is lifted for this backfill only,
-- as in 0009, and put back before the migration commits.
DROP TRIGGER events_no_update;
UPDATE events SET payload = json_set(
    payload, '$.supervisor', (SELECT supervisor_id FROM tasks WHERE tasks.id = events.task_id)
)
WHERE task_id IS NOT NULL
  AND json_type(payload) = 'object'
  AND json_extract(payload, '$.supervisor') IS NULL;
CREATE TRIGGER events_no_update BEFORE UPDATE ON events
BEGIN
    SELECT RAISE(ABORT, 'events are append-only');
END;
