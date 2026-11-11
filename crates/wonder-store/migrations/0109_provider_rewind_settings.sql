-- The owner's per-provider switches on this Mac. A missing row means the
-- setting is off, which is the default. `file_checkpointing`: the provider
-- keeps backups of files it edits in Wonder's chats so Edit from here can
-- restore them (Claude's file checkpointing).
CREATE TABLE provider_settings (
  agent_family TEXT NOT NULL CHECK (agent_family IN ('codex','claude','opencode')),
  setting TEXT NOT NULL CHECK (setting IN ('file_checkpointing')),
  enabled INTEGER NOT NULL CHECK (enabled = 1),
  updated_at TEXT NOT NULL,
  PRIMARY KEY (agent_family, setting)
);
