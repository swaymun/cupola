ALTER TABLE project_conversations ADD COLUMN unsandboxed_commands INTEGER NOT NULL DEFAULT 0 CHECK (unsandboxed_commands IN (0,1));
