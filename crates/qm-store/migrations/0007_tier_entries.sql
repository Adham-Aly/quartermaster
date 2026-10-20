-- A tier entry is a model plus an effort level, so one model can sit in the same tier more than
-- once at different efforts (Haiku at Medium and at High). `placements` was unique per
-- (canonical, tier); it becomes unique per (canonical, tier, effort), where no effort
-- (`effort_override` NULL, the tier default) counts as one value of its own. SQLite can't drop a
-- table's UNIQUE constraint, so the table is rebuilt; every existing row is kept as it is, ids and
-- order included.

CREATE TABLE placements_new (
    id              INTEGER PRIMARY KEY,
    canonical       TEXT    NOT NULL,
    tier            TEXT    NOT NULL CHECK (tier IN ('low', 'medium', 'high', 'premium')),
    effort_override TEXT    CHECK (effort_override IN ('low', 'medium', 'high', 'xhigh', 'max')),
    created_at      INTEGER NOT NULL
) STRICT;

INSERT INTO placements_new (id, canonical, tier, effort_override, created_at)
    SELECT id, canonical, tier, effort_override, created_at FROM placements;

DROP TABLE placements;
ALTER TABLE placements_new RENAME TO placements;

CREATE UNIQUE INDEX placements_entry
    ON placements (canonical, tier, coalesce(effort_override, ''));
