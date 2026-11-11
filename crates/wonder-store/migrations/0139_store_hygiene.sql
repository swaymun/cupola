-- Lookups made for each agent notification seek instead of scanning.
CREATE INDEX IF NOT EXISTS messages_codex_thread_turn
    ON messages(codex_thread_id, codex_turn_id, created_at);
-- Turn completion matches on the turn alone or on thread and turn.
CREATE INDEX IF NOT EXISTS assistant_messages_codex_turn_thread
    ON assistant_messages(codex_turn_id, codex_thread_id, created_at);
CREATE INDEX IF NOT EXISTS conversations_codex_thread
    ON conversations(codex_thread_id);
CREATE INDEX IF NOT EXISTS pending_app_server_notifications_thread
    ON pending_app_server_notifications(thread_id, received_at);

-- Periodic cleanup of expired sign-in sessions and spent action nonces.
CREATE INDEX IF NOT EXISTS sessions_expires_at
    ON sessions(expires_at_ms);
CREATE INDEX IF NOT EXISTS action_nonces_used_at
    ON action_nonces(used_at);

-- Search keeps every Project chat's documents and reindexes only the chats
-- whose searchable records changed. Archive and Project visibility are
-- applied when searching, so hiding or restoring a chat needs no reindex.
-- Trigger inserts check for an existing row rather than use OR IGNORE: an
-- upsert's own conflict clause would override it and fail the write.
CREATE TABLE search_stale_conversations (
    conversation_id TEXT PRIMARY KEY NOT NULL
) WITHOUT ROWID;
CREATE TABLE search_document_owners (
    doc_rowid INTEGER PRIMARY KEY,
    conversation_id TEXT NOT NULL
);
CREATE INDEX search_document_owners_conversation
    ON search_document_owners(conversation_id);

DELETE FROM search_documents;
INSERT OR IGNORE INTO search_stale_conversations(conversation_id)
    SELECT conversation_id FROM project_conversations;

CREATE TRIGGER search_stale_project_conversation_insert
AFTER INSERT ON project_conversations BEGIN
    INSERT INTO search_stale_conversations SELECT NEW.conversation_id WHERE NEW.conversation_id IS NOT NULL
        AND NOT EXISTS (SELECT 1 FROM search_stale_conversations WHERE conversation_id = NEW.conversation_id);
END;
CREATE TRIGGER search_stale_project_conversation_update
AFTER UPDATE OF conversation_id, title, updated_at ON project_conversations BEGIN
    INSERT INTO search_stale_conversations SELECT OLD.conversation_id WHERE OLD.conversation_id IS NOT NULL
        AND NOT EXISTS (SELECT 1 FROM search_stale_conversations WHERE conversation_id = OLD.conversation_id);
    INSERT INTO search_stale_conversations SELECT NEW.conversation_id WHERE NEW.conversation_id IS NOT NULL
        AND NOT EXISTS (SELECT 1 FROM search_stale_conversations WHERE conversation_id = NEW.conversation_id);
END;
CREATE TRIGGER search_stale_project_conversation_delete
AFTER DELETE ON project_conversations BEGIN
    INSERT INTO search_stale_conversations SELECT OLD.conversation_id WHERE OLD.conversation_id IS NOT NULL
        AND NOT EXISTS (SELECT 1 FROM search_stale_conversations WHERE conversation_id = OLD.conversation_id);
END;

CREATE TRIGGER search_stale_message_insert
AFTER INSERT ON messages BEGIN
    INSERT INTO search_stale_conversations SELECT NEW.conversation_id WHERE NEW.conversation_id IS NOT NULL
        AND NOT EXISTS (SELECT 1 FROM search_stale_conversations WHERE conversation_id = NEW.conversation_id);
END;
CREATE TRIGGER search_stale_message_update
AFTER UPDATE OF conversation_id, body, created_at ON messages BEGIN
    INSERT INTO search_stale_conversations SELECT OLD.conversation_id WHERE OLD.conversation_id IS NOT NULL
        AND NOT EXISTS (SELECT 1 FROM search_stale_conversations WHERE conversation_id = OLD.conversation_id);
    INSERT INTO search_stale_conversations SELECT NEW.conversation_id WHERE NEW.conversation_id IS NOT NULL
        AND NOT EXISTS (SELECT 1 FROM search_stale_conversations WHERE conversation_id = NEW.conversation_id);
END;
CREATE TRIGGER search_stale_message_delete
AFTER DELETE ON messages BEGIN
    INSERT INTO search_stale_conversations SELECT OLD.conversation_id WHERE OLD.conversation_id IS NOT NULL
        AND NOT EXISTS (SELECT 1 FROM search_stale_conversations WHERE conversation_id = OLD.conversation_id);
END;

CREATE TRIGGER search_stale_assistant_message_insert
AFTER INSERT ON assistant_messages BEGIN
    INSERT INTO search_stale_conversations SELECT NEW.conversation_id WHERE NEW.conversation_id IS NOT NULL
        AND NOT EXISTS (SELECT 1 FROM search_stale_conversations WHERE conversation_id = NEW.conversation_id);
END;
CREATE TRIGGER search_stale_assistant_message_update
AFTER UPDATE OF conversation_id, text, updated_at ON assistant_messages BEGIN
    INSERT INTO search_stale_conversations SELECT OLD.conversation_id WHERE OLD.conversation_id IS NOT NULL
        AND NOT EXISTS (SELECT 1 FROM search_stale_conversations WHERE conversation_id = OLD.conversation_id);
    INSERT INTO search_stale_conversations SELECT NEW.conversation_id WHERE NEW.conversation_id IS NOT NULL
        AND NOT EXISTS (SELECT 1 FROM search_stale_conversations WHERE conversation_id = NEW.conversation_id);
END;
CREATE TRIGGER search_stale_assistant_message_delete
AFTER DELETE ON assistant_messages BEGIN
    INSERT INTO search_stale_conversations SELECT OLD.conversation_id WHERE OLD.conversation_id IS NOT NULL
        AND NOT EXISTS (SELECT 1 FROM search_stale_conversations WHERE conversation_id = OLD.conversation_id);
END;

CREATE TRIGGER search_stale_conversation_file_insert
AFTER INSERT ON conversation_files BEGIN
    INSERT INTO search_stale_conversations SELECT NEW.conversation_id WHERE NEW.conversation_id IS NOT NULL
        AND NOT EXISTS (SELECT 1 FROM search_stale_conversations WHERE conversation_id = NEW.conversation_id);
END;
CREATE TRIGGER search_stale_conversation_file_update
AFTER UPDATE OF conversation_id, name, relative_path, updated_at ON conversation_files BEGIN
    INSERT INTO search_stale_conversations SELECT OLD.conversation_id WHERE OLD.conversation_id IS NOT NULL
        AND NOT EXISTS (SELECT 1 FROM search_stale_conversations WHERE conversation_id = OLD.conversation_id);
    INSERT INTO search_stale_conversations SELECT NEW.conversation_id WHERE NEW.conversation_id IS NOT NULL
        AND NOT EXISTS (SELECT 1 FROM search_stale_conversations WHERE conversation_id = NEW.conversation_id);
END;
CREATE TRIGGER search_stale_conversation_file_delete
AFTER DELETE ON conversation_files BEGIN
    INSERT INTO search_stale_conversations SELECT OLD.conversation_id WHERE OLD.conversation_id IS NOT NULL
        AND NOT EXISTS (SELECT 1 FROM search_stale_conversations WHERE conversation_id = OLD.conversation_id);
END;
