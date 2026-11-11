-- The most detailed chat style also shows Codex's thinking summaries.
CREATE TABLE thinking_agent_family_settings (
  agent_family TEXT PRIMARY KEY NOT NULL CHECK (agent_family IN ('codex','claude','opencode','cursor')),
  thread_tools INTEGER NOT NULL DEFAULT 1 CHECK (thread_tools IN (0,1)),
  chat_detail TEXT NOT NULL DEFAULT 'everything' CHECK (chat_detail IN ('everything','conversation','thinking')),
  instruction_enabled INTEGER NOT NULL DEFAULT 0 CHECK (instruction_enabled IN (0,1)),
  instruction TEXT NOT NULL DEFAULT '' CHECK (length(instruction) <= 4000),
  updated_at TEXT NOT NULL
);

INSERT INTO thinking_agent_family_settings
  SELECT agent_family,thread_tools,chat_detail,instruction_enabled,instruction,updated_at
  FROM agent_family_settings;

DROP TABLE agent_family_settings;

ALTER TABLE thinking_agent_family_settings RENAME TO agent_family_settings;
