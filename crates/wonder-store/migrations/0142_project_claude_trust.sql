-- A Project folder's own Claude Code configuration (hooks, permission rules,
-- MCP servers, helpers and the files they run) that the owner chose to trust.
-- Each row is one fingerprint the Claude runtime computed over the resolved
-- folders and that content; anything else, including a later change, runs
-- with the owner's own Claude settings only until it is trusted again.
CREATE TABLE project_claude_trust (
  project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
  fingerprint TEXT NOT NULL CHECK (length(fingerprint) = 64 AND fingerprint NOT GLOB '*[^0-9a-f]*'),
  trusted_at TEXT NOT NULL,
  PRIMARY KEY (project_id, fingerprint)
);
