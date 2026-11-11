CREATE TABLE async_questions (
 id TEXT PRIMARY KEY,
 conversation_id TEXT NOT NULL,
 thread_id TEXT NOT NULL,
 turn_id TEXT NOT NULL,
 item_id TEXT NOT NULL,
 questions_json TEXT NOT NULL,
 state TEXT NOT NULL DEFAULT 'pending',
 expires_at_ms INTEGER NOT NULL,
 response_json TEXT,
 message_id TEXT,
 UNIQUE(thread_id, turn_id, item_id)
);
CREATE TRIGGER sync_async_questions_insert AFTER INSERT ON async_questions BEGIN
 INSERT INTO sync_journal(host_epoch,occurred_at,conversation_id,resource,change_kind)
 SELECT host_epoch,strftime('%Y-%m-%dT%H:%M:%fZ','now'),NEW.conversation_id,'async_questions','insert' FROM sync_state WHERE singleton=1;
END;
CREATE TRIGGER sync_async_questions_update AFTER UPDATE ON async_questions BEGIN
 INSERT INTO sync_journal(host_epoch,occurred_at,conversation_id,resource,change_kind)
 SELECT host_epoch,strftime('%Y-%m-%dT%H:%M:%fZ','now'),NEW.conversation_id,'async_questions','update' FROM sync_state WHERE singleton=1;
END;
