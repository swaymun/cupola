-- `desktop_computer_use` (Codex): Wonder's Codex threads use the ChatGPT
-- desktop app's own computer use instead of Cua Driver, the default. SQLite
-- cannot widen a CHECK in place, so the table is rebuilt with its rows.
CREATE TABLE provider_settings_new (
  agent_family TEXT NOT NULL CHECK (agent_family IN ('codex','claude','opencode')),
  setting TEXT NOT NULL CHECK (setting IN ('file_checkpointing','desktop_computer_use')),
  enabled INTEGER NOT NULL CHECK (enabled = 1),
  updated_at TEXT NOT NULL,
  PRIMARY KEY (agent_family, setting)
);
INSERT INTO provider_settings_new SELECT agent_family, setting, enabled, updated_at FROM provider_settings;
DROP TABLE provider_settings;
ALTER TABLE provider_settings_new RENAME TO provider_settings;
