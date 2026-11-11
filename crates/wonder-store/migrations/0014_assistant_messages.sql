CREATE TABLE IF NOT EXISTS assistant_messages (
    id TEXT PRIMARY KEY NOT NULL,
    conversation_id TEXT NOT NULL,
    codex_thread_id TEXT NOT NULL,
    codex_turn_id TEXT NOT NULL,
    item_id TEXT NOT NULL,
    text TEXT NOT NULL,
    state TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE (conversation_id, codex_turn_id, item_id)
);

CREATE TABLE IF NOT EXISTS assistant_message_deltas (
    assistant_message_id TEXT NOT NULL REFERENCES assistant_messages(id) ON DELETE CASCADE,
    delta_sha256 TEXT NOT NULL,
    PRIMARY KEY (assistant_message_id, delta_sha256)
);

CREATE INDEX IF NOT EXISTS assistant_messages_conversation_idx
    ON assistant_messages(conversation_id, created_at ASC);
