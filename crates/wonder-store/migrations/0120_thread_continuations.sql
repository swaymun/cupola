-- Continuing work without the owner: resume a thread stopped by a usage limit
-- once the limit resets, and continue a reply cut off by a crash or restart.
-- Both are host-wide and on by default; each scheduled resume can be cancelled.
INSERT OR IGNORE INTO project_settings(id, include_worktrees) VALUES (1, 1);
ALTER TABLE project_settings ADD COLUMN resume_after_limit INTEGER NOT NULL DEFAULT 1
  CHECK (resume_after_limit IN (0, 1));
ALTER TABLE project_settings ADD COLUMN continue_after_restart INTEGER NOT NULL DEFAULT 1
  CHECK (continue_after_restart IN (0, 1));

-- One scheduled resume per conversation, at `resume_at` (Unix seconds).
-- `stopped_message_id` is the send the limit stopped: any later message in
-- the conversation means the owner continued, and the resume is dropped.
CREATE TABLE usage_resumes (
  conversation_id TEXT PRIMARY KEY NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
  stopped_turn_id TEXT NOT NULL,
  stopped_message_id TEXT NOT NULL,
  resume_at INTEGER NOT NULL,
  created_at TEXT NOT NULL
);
-- Counts a thread's archives, so a resume whose reset was still being looked
-- up when the thread was archived (and maybe restored) is not scheduled.
CREATE TABLE thread_archive_counts (
  conversation_id TEXT PRIMARY KEY NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
  archive_count INTEGER NOT NULL
);

-- A Project's defaults for new threads; no row starts from the phone's last
-- choice. `model`/`effort` apply to `agent_family`.
CREATE TABLE project_thread_defaults (
  project_id TEXT PRIMARY KEY NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  agent_family TEXT NOT NULL,
  model TEXT,
  effort TEXT,
  access_mode TEXT CHECK (access_mode IS NULL OR access_mode IN ('read_only','workspace','full_access'))
);
