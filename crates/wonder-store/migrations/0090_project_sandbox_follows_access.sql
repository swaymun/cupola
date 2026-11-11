-- Full access now means a Claude Project runs commands with no sandbox. A thread
-- that chose Full access but kept commands sandboxed moves to Auto, which keeps
-- the sandbox, instead of silently gaining unrestricted commands.
UPDATE project_conversations SET access_mode='workspace', claude_approval='auto'
WHERE agent_family='claude' AND access_mode='full_access' AND unsandboxed_commands=0;
ALTER TABLE project_conversations DROP COLUMN unsandboxed_commands;
