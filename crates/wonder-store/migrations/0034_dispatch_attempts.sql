-- Explicit membership prevents Guide messages becoming direct sends.
CREATE TABLE dispatch_work (
    message_id TEXT PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE
);
CREATE TABLE dispatch_attempts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    message_id TEXT NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    phase TEXT NOT NULL,
    thread_id TEXT,
    turn_id TEXT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX dispatch_attempt_message ON dispatch_attempts(message_id, id);
CREATE INDEX messages_pending_dispatch ON messages(state, created_at, id);
-- Older rows lack durable routing and submission evidence. Do not guess their intent.
UPDATE messages SET state = 'uncertain' WHERE state IN ('accepted_by_wonder', 'dispatching_to_codex');
