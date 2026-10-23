-- 0016: where an in-house agent's chat on disk starts. The visible chat (`messages`,
-- `session_lines`, `turns`) of an agent on the app's own provider engine is kept in state.db; a
-- Claude agent's stays in memory. `chat_kept_from` is the `seq` its kept chat starts at: set when
-- the agent starts on the provider engine (its own `seq` when it never ran on another), NULL on any
-- other engine, so rows made under a Claude session before a switch never reach the disk.
ALTER TABLE agents ADD COLUMN chat_kept_from INTEGER;

-- Agents already on the provider engine: their chat was never on disk, so it starts anywhere.
UPDATE agents SET chat_kept_from = seq WHERE engine = 'provider' AND retired_at IS NULL;
