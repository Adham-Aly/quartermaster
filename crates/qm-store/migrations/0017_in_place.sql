-- 0017: whether a task works in place, in the folder itself, rather than in a worktree. An in-place
-- task has no branch, no app commit and no merge: its edits are final. Plain-folder tasks always
-- work in place; a git project's only when the user's own words ask for it.
ALTER TABLE tasks ADD COLUMN in_place INTEGER NOT NULL DEFAULT 0 CHECK (in_place IN (0, 1));

-- Tasks already working in a folder (the old personal workspace) work in place.
UPDATE tasks SET in_place = 1 WHERE delivery_mode = 'folder';
