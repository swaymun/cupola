CREATE TABLE IF NOT EXISTS pending_app_server_notifications (
    id TEXT PRIMARY KEY NOT NULL,
    identity_key TEXT UNIQUE,
    method TEXT NOT NULL,
    thread_id TEXT,
    turn_id TEXT,
    item_id TEXT,
    params_json TEXT NOT NULL,
    received_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS pending_app_server_notifications_turn_idx
    ON pending_app_server_notifications(turn_id, received_at ASC);
