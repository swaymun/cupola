-- Existing chats keep today's notification behavior; no content is removed.
ALTER TABLE project_conversations ADD COLUMN notification_level TEXT NOT NULL DEFAULT 'default'
    CHECK(notification_level IN ('default','follow','mute'));

-- Muting cancels every push still pending for the chat. A delivery being sent
-- is still pending until it succeeds, so it is cancelled too.
CREATE TRIGGER chat_notification_mute AFTER UPDATE OF notification_level ON project_conversations
WHEN NEW.notification_level='mute'
BEGIN
    UPDATE push_outbox SET state='cancelled'
    WHERE conversation_id=NEW.conversation_id AND state='pending';
END;
