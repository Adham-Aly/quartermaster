-- The parent channel pump's read position: the seq of the newest event it has read. Kept
-- across restarts, so a fact the pump hasn't read yet keeps its task and request (kept.rs).
-- One row: the mirror keys every kept row by `seq`, so that key is fixed at 1.
CREATE TABLE parent_channel_cursor (
    seq       INTEGER NOT NULL UNIQUE CHECK (seq = 1),
    event_seq INTEGER NOT NULL
) STRICT;
