-- A finished answer task's report has a card in the Actions panel until the user opens or dismisses
-- it. This is when they last did, in milliseconds; NULL until then. A newer `reported_at` (the task
-- reported again) brings the card back.
ALTER TABLE tasks ADD COLUMN report_read_at INTEGER;
