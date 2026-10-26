-- An orchestrator may choose which of the user's messages a brief or routed request carries, and
-- leave out whole passages of it that are about other work. These hold the passages left out, as a
-- JSON array of strings, so what was trimmed stays on record next to the trimmed words; NULL when
-- the words are the message as it was.
ALTER TABLE tasks ADD COLUMN user_words_cuts TEXT
    CHECK (user_words_cuts IS NULL OR json_valid(user_words_cuts));
ALTER TABLE secondmate_requests ADD COLUMN user_words_cuts TEXT
    CHECK (user_words_cuts IS NULL OR json_valid(user_words_cuts));
