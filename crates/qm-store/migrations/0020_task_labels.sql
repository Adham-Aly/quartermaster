-- Task labels: a task (`T-`) groups crewmates (`C-`), and a crewmate's task can change. Each
-- `tasks` row is one crewmate's work; `label_id` names its task and `folder_label_id` the task
-- whose folders hold its artifacts and scratchpad (they differ only while a relabel waits to move
-- them). Every row made before this was its own task, under the same number.
CREATE TABLE task_labels (
    id INTEGER PRIMARY KEY,
    created_at INTEGER NOT NULL,
    seq INTEGER NOT NULL
) STRICT;
CREATE UNIQUE INDEX task_labels_seq ON task_labels (seq);

INSERT INTO task_labels (id, created_at, seq) SELECT id, created_at, seq FROM tasks;

ALTER TABLE tasks ADD COLUMN label_id INTEGER REFERENCES task_labels (id);
ALTER TABLE tasks ADD COLUMN folder_label_id INTEGER REFERENCES task_labels (id);
UPDATE tasks SET label_id = id, folder_label_id = id;
CREATE INDEX tasks_label ON tasks (label_id);
