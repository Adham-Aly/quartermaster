-- 0013: autonomy becomes two modes, `normal` (was `off`) and `auto` (was `on`). The meaning of
-- each is unchanged for settling; the modes also decide how plan tasks plan. SQLite can't alter a
-- CHECK, so `projects` is rebuilt exactly as 0008 left it plus 0009's `seq`, and every row keeps
-- its id. Foreign keys point at `projects` by name. `state.db` is migrated with foreign keys off
-- (`connection::open_disk`) and the working database migrates empty, so the drop counts no
-- violation; `foreign_key_check` runs after.

PRAGMA defer_foreign_keys = ON;

CREATE TABLE projects_new (
    id                 INTEGER PRIMARY KEY,
    name               TEXT    NOT NULL,
    path               TEXT    NOT NULL,
    origin             TEXT    NOT NULL CHECK (origin IN ('linked', 'cloned', 'created', 'workspace')),
    default_branch     TEXT    NOT NULL,
    github_remote      TEXT,
    delivery_mode      TEXT    NOT NULL CHECK (delivery_mode IN ('direct_pr', 'branch_only', 'build_in_place', 'folder')),
    autonomy           TEXT    NOT NULL DEFAULT 'normal' CHECK (autonomy IN ('normal', 'auto')),
    verification_level TEXT    NOT NULL DEFAULT 'standard' CHECK (verification_level IN ('quick', 'standard', 'reviewed')),
    merge_method       TEXT    NOT NULL CHECK (merge_method IN ('merge', 'squash', 'rebase')),
    files_to_copy      TEXT    NOT NULL DEFAULT '[]' CHECK (json_valid(files_to_copy) AND json_type(files_to_copy) = 'array'),
    setup_command      TEXT,
    created_at         INTEGER NOT NULL,
    removed_at         INTEGER,
    seq                INTEGER NOT NULL DEFAULT 0
) STRICT;

INSERT INTO projects_new (id, name, path, origin, default_branch, github_remote, delivery_mode,
                          autonomy, verification_level, merge_method, files_to_copy, setup_command,
                          created_at, removed_at, seq)
    SELECT id, name, path, origin, default_branch, github_remote, delivery_mode,
           CASE autonomy WHEN 'on' THEN 'auto' ELSE 'normal' END,
           verification_level, merge_method, files_to_copy, setup_command,
           created_at, removed_at, seq FROM projects;

DROP TABLE projects;
ALTER TABLE projects_new RENAME TO projects;

CREATE UNIQUE INDEX projects_name_active ON projects (name) WHERE removed_at IS NULL;
CREATE UNIQUE INDEX projects_path_active ON projects (path) WHERE removed_at IS NULL;
CREATE UNIQUE INDEX projects_one_workspace ON projects (origin) WHERE origin = 'workspace';
CREATE UNIQUE INDEX projects_seq ON projects (seq);
