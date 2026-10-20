-- 0005: verification.

-- The declared diagnoses and the checks the last push meant to fix
-- ({"declared":[...],"awaiting":[...]}); the counts stay in repair_attempts and flake_rerun_used.
ALTER TABLE tasks ADD COLUMN repair_pending TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(repair_pending));
ALTER TABLE tasks ADD COLUMN repair_held INTEGER NOT NULL DEFAULT 0 CHECK (repair_held IN (0, 1));
-- Careful: when the independent review passed.
ALTER TABLE tasks ADD COLUMN review_passed_at INTEGER;
-- The level was raised and the missing validation hasn't been reported yet.
ALTER TABLE tasks ADD COLUMN validation_pending INTEGER NOT NULL DEFAULT 0 CHECK (validation_pending IN (0, 1));
