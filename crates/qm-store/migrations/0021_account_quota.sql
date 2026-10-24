-- Quota-aware routing. `quota` holds an account's provider quota pools from the Settings quota
-- checks (JSON of `qm_core`'s stored pools, numbers and reset times only), read at
-- `quota_read_at`. `reserve_at` is your reserve mark: the percent of its largest pool USED at
-- which routing holds the account back in favour of accounts under theirs. NULL: no mark.
ALTER TABLE accounts ADD COLUMN quota TEXT CHECK (quota IS NULL OR json_valid(quota));
ALTER TABLE accounts ADD COLUMN quota_read_at INTEGER;
ALTER TABLE accounts ADD COLUMN reserve_at INTEGER
    CHECK (reserve_at IS NULL OR reserve_at BETWEEN 1 AND 99);
