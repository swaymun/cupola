-- Claude app sync (Claude only): a row means the owner turned it on.
CREATE TABLE claude_app_provider_settings (
  agent_family TEXT NOT NULL CHECK (agent_family IN ('codex','claude','opencode','cursor')),
  setting TEXT NOT NULL CHECK (setting IN ('file_checkpointing','desktop_computer_use','claude_app_sync')),
  enabled INTEGER NOT NULL CHECK (enabled = 1),
  updated_at TEXT NOT NULL,
  PRIMARY KEY (agent_family, setting),
  CHECK (setting <> 'claude_app_sync' OR agent_family = 'claude')
);

INSERT INTO claude_app_provider_settings SELECT agent_family,setting,enabled,updated_at FROM provider_settings;

DROP TABLE provider_settings;

ALTER TABLE claude_app_provider_settings RENAME TO provider_settings;
