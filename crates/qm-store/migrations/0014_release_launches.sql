-- 0014: release launches. A crewmate proposes a release tag (`propose_release_tag`); you approve
-- it on its own card; the app creates and pushes exactly that tag at exactly that commit, never
-- overwriting one. A row binds the approval to the project, the repository (GitHub's numeric id
-- and its name), the tag and the commit, and records what the remote showed afterwards.
--
-- `status` moves only forward. The single compare-and-swap from 'proposed' to 'pushing' is the
-- only way a tag is ever sent, so a retry or a restart never sends it twice. The partial unique
-- index keeps one live launch per tag and repository. Every link is nullable: a forgotten task,
-- agent or card leaves the launch, which carries its own binding.

CREATE TABLE release_launches (
    id             INTEGER PRIMARY KEY,
    seq            INTEGER NOT NULL,
    task_id        INTEGER REFERENCES tasks (id),
    project_id     INTEGER REFERENCES projects (id),
    held_id        INTEGER REFERENCES held_actions (id),
    asked_by       INTEGER REFERENCES agents (id),
    -- `owner/name` as GitHub showed it when the launch was proposed, and GitHub's numeric id.
    repo           TEXT    NOT NULL,
    repo_id        INTEGER NOT NULL,
    tag            TEXT    NOT NULL CHECK (length(tag) BETWEEN 1 AND 128),
    commit_sha     TEXT    NOT NULL CHECK (length(commit_sha) IN (40, 64)),
    -- The branch the commit was checked against, if the crewmate named one.
    branch         TEXT,
    tag_kind       TEXT    NOT NULL CHECK (tag_kind IN ('annotated', 'lightweight')),
    message        TEXT,
    status         TEXT    NOT NULL DEFAULT 'proposed' CHECK (status IN ('proposed', 'pushing', 'unconfirmed', 'published', 'collision', 'rejected', 'not_published', 'superseded', 'declined')),
    -- The tag object the app wrote (annotated tags), so a retry recognises its own tag.
    tag_object     TEXT,
    -- What the remote's tag ref pointed at when last read.
    remote_object  TEXT,
    -- The last outcome in plain words.
    remote_words   TEXT,
    -- Release observation after publication, kept for later use; unused until then.
    observe_state  TEXT,
    observation    TEXT    CHECK (observation IS NULL OR json_valid(observation)),
    proposed_at    INTEGER NOT NULL,
    pushed_at      INTEGER,
    published_at   INTEGER,
    observed_at    INTEGER,
    ended_at       INTEGER,
    CHECK ((tag_kind = 'annotated') OR (message IS NULL))
) STRICT;

CREATE UNIQUE INDEX release_launches_seq ON release_launches (seq);
CREATE UNIQUE INDEX release_launches_live ON release_launches (repo_id, tag)
    WHERE status IN ('proposed', 'pushing', 'unconfirmed', 'published');
