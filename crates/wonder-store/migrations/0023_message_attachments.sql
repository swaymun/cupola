CREATE TABLE IF NOT EXISTS message_attachments (
    message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    file_id TEXT NOT NULL REFERENCES conversation_files(id) ON DELETE CASCADE,
    created_at TEXT NOT NULL,
    PRIMARY KEY (message_id, file_id)
);

CREATE INDEX IF NOT EXISTS message_attachments_file_idx
    ON message_attachments(file_id, message_id);
