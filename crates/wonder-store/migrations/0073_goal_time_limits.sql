-- Only user-specified active-time limits are stored here. Goal state belongs to
-- the Codex runtime and is recovered with thread/goal/get.
CREATE TABLE goal_threads (
  conversation_id TEXT PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE,
  thread_id TEXT NOT NULL UNIQUE
);
CREATE TABLE goal_turns (
  thread_id TEXT NOT NULL,
  turn_id TEXT NOT NULL,
  conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
  PRIMARY KEY(thread_id,turn_id)
);
CREATE TABLE goal_time_limits (
  conversation_id TEXT PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE,
  thread_id TEXT NOT NULL,
  budget_seconds INTEGER NOT NULL
);
