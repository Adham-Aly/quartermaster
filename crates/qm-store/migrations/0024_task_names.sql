-- A task's name: two or three plain words the crew list shows for it ("Token refresh race"), as
-- the orchestrator gave it when it started the task. NULL when none was given; the crew list then
-- makes one from its oldest crewmate's title.
ALTER TABLE task_labels ADD COLUMN name TEXT;
