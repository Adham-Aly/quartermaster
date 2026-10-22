-- Explicit forgetting delivers minimal parent facts independently of tasks and requests.
-- Markers also dedupe pump batches already read before the forgetting transaction.
-- No wake FK: consumed wakes can be pruned independently by the disk mirror.
CREATE TABLE forgotten_parent_facts (
    id INTEGER PRIMARY KEY,
    wake_id INTEGER,
    seq INTEGER NOT NULL UNIQUE
) STRICT;
