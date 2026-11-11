-- A Claude Code thread archived from Wonder is hidden in Wonder only: Claude Code
-- has no archive, so its session stays untouched on the Mac. Codex threads keep
-- using Codex's own archive and never set this column.
ALTER TABLE project_conversations ADD COLUMN wonder_archived_at TEXT;
