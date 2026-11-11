-- A delegated task's own result, recorded once when its work first ends: how
-- it ended and the child's final reply. The parent reads it from here, so it
-- survives a restart and does not depend on the provider's history.
ALTER TABLE thread_delegations ADD COLUMN result_outcome TEXT;
ALTER TABLE thread_delegations ADD COLUMN result_text TEXT;
ALTER TABLE thread_delegations ADD COLUMN result_truncated INTEGER NOT NULL DEFAULT 0;
ALTER TABLE thread_delegations ADD COLUMN result_at TEXT;
