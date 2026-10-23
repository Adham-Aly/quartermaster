-- 0014: which agent's current conversation got each pasted image's folder. Images now live in the
-- data directory, which no /tmp cleaner empties, so the app must know when a folder is still part
-- of a chat that survives a restart. A row is written with the message that carried the image and
-- removed when that agent's conversation is cleared or the agent retires. `message_id` has no
-- foreign key because messages are not kept on disk.

CREATE TABLE attachment_uses (
    message_id INTEGER NOT NULL,
    agent_id   INTEGER NOT NULL REFERENCES agents (id),
    created_at INTEGER NOT NULL,
    seq        INTEGER NOT NULL,
    PRIMARY KEY (message_id, agent_id)
) STRICT;
CREATE INDEX attachment_uses_agent ON attachment_uses (agent_id);
CREATE UNIQUE INDEX attachment_uses_seq ON attachment_uses (seq);
