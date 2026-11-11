-- What Wonder adds to its own sessions of one provider, as the owner set it
-- on the Mac or the phone. A missing row means every default: Wonder's thread
-- tools on (as before this table), every activity shown in chats, and no
-- custom instruction. The instruction text is kept while it is off.
CREATE TABLE agent_family_settings (
  agent_family TEXT PRIMARY KEY NOT NULL CHECK (agent_family IN ('codex','claude','opencode')),
  thread_tools INTEGER NOT NULL DEFAULT 1 CHECK (thread_tools IN (0,1)),
  chat_detail TEXT NOT NULL DEFAULT 'everything' CHECK (chat_detail IN ('everything','conversation')),
  instruction_enabled INTEGER NOT NULL DEFAULT 0 CHECK (instruction_enabled IN (0,1)),
  instruction TEXT NOT NULL DEFAULT '' CHECK (length(instruction) <= 4000),
  updated_at TEXT NOT NULL
);
