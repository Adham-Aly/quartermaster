-- User-message provenance is independent of engine chat retention. A model choice may cite an
-- earlier user message after a restart, even when Claude owns the visible conversation.
-- No FK to messages: that row may legitimately be forgotten. IDs remain reserved while kept.
CREATE TABLE user_message_sources (
    id INTEGER PRIMARY KEY,
    agent_id INTEGER NOT NULL REFERENCES agents(id),
    text TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL,
    seq INTEGER NOT NULL UNIQUE
) STRICT;
CREATE INDEX user_message_sources_agent ON user_message_sources(agent_id, seq);
CREATE TRIGGER user_message_sources_no_update BEFORE UPDATE ON user_message_sources
BEGIN SELECT RAISE(ABORT, 'user-message provenance is immutable'); END;
-- Capture the full original words, before attachment-path rewriting.
-- Backfill only retained current chat, never discarded legacy conversation text.
INSERT INTO user_message_sources (id, agent_id, text, created_at, seq)
SELECT m.id, m.agent_id, m.text, m.created_at, m.seq
FROM messages m WHERE source = 'you' AND kind = 'message'
    AND EXISTS (SELECT 1 FROM agents a WHERE a.id = m.agent_id
                AND a.retired_at IS NULL AND a.engine = 'provider'
                AND m.seq >= a.chat_kept_from);
