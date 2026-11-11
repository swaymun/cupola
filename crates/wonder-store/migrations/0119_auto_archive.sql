-- Auto-archive: archive Project threads after N idle days (0 = off) and when
-- their pull request merges. `auto_archive_since` is when the rule last turned
-- on; a thread's idle time never counts from before it, so turning the rule on
-- (or upgrading) archives nothing at once.
INSERT OR IGNORE INTO project_settings(id, include_worktrees) VALUES (1, 1);
ALTER TABLE project_settings ADD COLUMN auto_archive_days INTEGER NOT NULL DEFAULT 3
  CHECK (auto_archive_days BETWEEN 0 AND 90);
ALTER TABLE project_settings ADD COLUMN auto_archive_on_merge INTEGER NOT NULL DEFAULT 1
  CHECK (auto_archive_on_merge IN (0, 1));
ALTER TABLE project_settings ADD COLUMN auto_archive_since TEXT;
UPDATE project_settings SET auto_archive_since = strftime('%Y-%m-%dT%H:%M:%fZ', 'now');

-- A Project's own rule; no row means it follows the host-wide rule.
CREATE TABLE project_auto_archive (
  project_id TEXT PRIMARY KEY NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  idle_days INTEGER NOT NULL CHECK (idle_days BETWEEN 0 AND 90),
  on_merge INTEGER NOT NULL CHECK (on_merge IN (0, 1)),
  since TEXT NOT NULL
);
