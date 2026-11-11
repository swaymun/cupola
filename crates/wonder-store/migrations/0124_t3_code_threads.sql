-- Threads T3 Code owns (owner, Oct 21): one T3 Code started or knows keeps
-- going through T3 Code whenever T3 Code runs, also after Cupola restarts
-- while T3 Code is closed. Kept for the native session it was matched with:
-- a reset or switched session is no longer T3 Code's.
CREATE TABLE t3_code_threads (
  conversation_id TEXT PRIMARY KEY NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
  native_session_id TEXT NOT NULL,
  t3_thread_id TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
