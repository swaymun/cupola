-- A forked thread remembers the thread it was copied from and the turn it was
-- cut at (null: the whole conversation). The source is never changed.
CREATE TABLE project_conversation_forks (
  conversation_id TEXT PRIMARY KEY NOT NULL REFERENCES project_conversations(conversation_id) ON DELETE CASCADE,
  forked_from TEXT NOT NULL,
  forked_at_turn TEXT
);
