ALTER TABLE messages ADD COLUMN queue_revision INTEGER NOT NULL DEFAULT 1;
ALTER TABLE messages ADD COLUMN queue_position INTEGER NOT NULL DEFAULT 0;
ALTER TABLE messages ADD COLUMN original_body_sha256 TEXT;
UPDATE messages SET original_body_sha256 = body_sha256;
CREATE TRIGGER message_original_body AFTER INSERT ON messages
BEGIN UPDATE messages SET original_body_sha256 = NEW.body_sha256 WHERE id = NEW.id; END;
CREATE INDEX queue_conversation ON messages(conversation_id, state, queue_position, created_at, id);
CREATE TABLE guide_work (
    message_id TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
    expected_turn_id TEXT NOT NULL
);
