-- A chat may move to a git worktree of its own repository (the host holds the
-- chat idle and inserts a guard row for the one UPDATE); nothing else changes
-- a chat's project or working folder.
CREATE TABLE project_folder_move_guard (
 conversation_id TEXT PRIMARY KEY NOT NULL
) WITHOUT ROWID;

DROP TRIGGER project_conversation_identity_is_fixed;
CREATE TRIGGER project_conversation_identity_is_fixed BEFORE UPDATE OF
 project_id, agent_family, provider_store, cwd ON project_conversations
WHEN (NEW.project_id != OLD.project_id
 OR (NEW.cwd != OLD.cwd
  AND NOT EXISTS(SELECT 1 FROM project_folder_move_guard WHERE conversation_id=OLD.conversation_id))
 OR ((NEW.agent_family != OLD.agent_family OR NEW.provider_store != OLD.provider_store)
  AND NOT EXISTS(SELECT 1 FROM project_runtime_switch_guard WHERE conversation_id=OLD.conversation_id)))
BEGIN
 SELECT RAISE(ABORT,'A project conversation keeps its provider identity and working folder');
END;
