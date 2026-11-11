-- A deleted Cupola chat never deletes provider-owned history or project files.
-- Keep only opaque IDs to suppress rediscovery, including sessions left by a switch.
CREATE TABLE deleted_project_chats (conversation_id TEXT PRIMARY KEY NOT NULL, deleted_at TEXT NOT NULL);
CREATE TABLE deleted_project_sessions (
 agent_family TEXT NOT NULL, provider_store TEXT NOT NULL, native_session_id TEXT NOT NULL,
 PRIMARY KEY(agent_family, provider_store, native_session_id)
);
CREATE TRIGGER deleted_chat_blocks_conversations BEFORE INSERT ON conversations
WHEN EXISTS(SELECT 1 FROM deleted_project_chats WHERE conversation_id=NEW.id)
BEGIN SELECT RAISE(ABORT,'This chat was deleted from Cupola'); END;
CREATE TRIGGER deleted_chat_blocks_messages BEFORE INSERT ON messages
WHEN EXISTS(SELECT 1 FROM deleted_project_chats WHERE conversation_id=NEW.conversation_id)
BEGIN SELECT RAISE(ABORT,'This chat was deleted from Cupola'); END;
CREATE TRIGGER deleted_chat_blocks_assistant_messages BEFORE INSERT ON assistant_messages
WHEN EXISTS(SELECT 1 FROM deleted_project_chats WHERE conversation_id=NEW.conversation_id)
BEGIN SELECT RAISE(ABORT,'This chat was deleted from Cupola'); END;
CREATE TRIGGER deleted_chat_blocks_conversation_files BEFORE INSERT ON conversation_files
WHEN EXISTS(SELECT 1 FROM deleted_project_chats WHERE conversation_id=NEW.conversation_id)
BEGIN SELECT RAISE(ABORT,'This chat was deleted from Cupola'); END;
CREATE TRIGGER deleted_chat_blocks_history_entries BEFORE INSERT ON history_entries
WHEN EXISTS(SELECT 1 FROM deleted_project_chats WHERE conversation_id=NEW.conversation_id)
BEGIN SELECT RAISE(ABORT,'This chat was deleted from Cupola'); END;
CREATE TRIGGER deleted_chat_blocks_history_hydration BEFORE INSERT ON history_hydration
WHEN EXISTS(SELECT 1 FROM deleted_project_chats WHERE conversation_id=NEW.conversation_id)
BEGIN SELECT RAISE(ABORT,'This chat was deleted from Cupola'); END;
CREATE TRIGGER deleted_chat_blocks_async_questions BEFORE INSERT ON async_questions
WHEN EXISTS(SELECT 1 FROM deleted_project_chats WHERE conversation_id=NEW.conversation_id)
BEGIN SELECT RAISE(ABORT,'This chat was deleted from Cupola'); END;
CREATE TRIGGER deleted_chat_blocks_approvals BEFORE INSERT ON approvals
WHEN EXISTS(SELECT 1 FROM deleted_project_chats WHERE conversation_id=NEW.conversation_id)
BEGIN SELECT RAISE(ABORT,'This chat was deleted from Cupola'); END;
CREATE TRIGGER deleted_chat_blocks_push_intents BEFORE INSERT ON push_intents
WHEN EXISTS(SELECT 1 FROM deleted_project_chats WHERE conversation_id=NEW.conversation_id)
BEGIN SELECT RAISE(ABORT,'This chat was deleted from Cupola'); END;
CREATE TRIGGER deleted_session_blocks_project_conversations BEFORE INSERT ON project_conversations
WHEN EXISTS(SELECT 1 FROM deleted_project_sessions WHERE agent_family=NEW.agent_family AND provider_store=NEW.provider_store AND native_session_id=NEW.native_session_id)
BEGIN SELECT RAISE(ABORT,'This chat was deleted from Cupola'); END;
CREATE TRIGGER deleted_session_blocks_project_runtime_history BEFORE INSERT ON project_runtime_history
WHEN EXISTS(SELECT 1 FROM deleted_project_sessions WHERE agent_family=NEW.agent_family AND provider_store=NEW.provider_store AND native_session_id=NEW.native_session_id)
BEGIN SELECT RAISE(ABORT,'This chat was deleted from Cupola'); END;
CREATE TRIGGER deleted_session_blocks_assignment BEFORE UPDATE OF native_session_id ON project_conversations
WHEN EXISTS(SELECT 1 FROM deleted_project_sessions WHERE agent_family=NEW.agent_family AND provider_store=NEW.provider_store AND native_session_id=NEW.native_session_id)
BEGIN SELECT RAISE(ABORT,'This chat was deleted from Cupola'); END;
CREATE TRIGGER deleted_chat_blocks_events BEFORE INSERT ON events
WHEN EXISTS(SELECT 1 FROM deleted_project_chats WHERE conversation_id=COALESCE(json_extract(NEW.payload_json,'$.conversationId'),json_extract(NEW.payload_json,'$.conversation_id')))
BEGIN SELECT RAISE(ABORT,'This chat was deleted from Cupola'); END;
-- Delete invalidations must still reach other paired devices. They contain
-- no history payload and are produced by the removal transaction itself.
CREATE TRIGGER deleted_chat_blocks_journal BEFORE INSERT ON sync_journal
WHEN NEW.change_kind != 'delete' AND EXISTS(SELECT 1 FROM deleted_project_chats WHERE conversation_id=NEW.conversation_id)
BEGIN SELECT RAISE(ABORT,'This chat was deleted from Cupola'); END;
