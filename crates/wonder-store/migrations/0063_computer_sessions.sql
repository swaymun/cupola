CREATE TABLE computer_sessions (
    id TEXT PRIMARY KEY,
    client_request_id TEXT NOT NULL,
    owner_device_id TEXT NOT NULL,
    host_installation_id TEXT NOT NULL,
    conversation_id TEXT NOT NULL,
    generation INTEGER NOT NULL,
    state TEXT NOT NULL,
    source_id TEXT,
    source_name TEXT,
    source_kind TEXT,
    source_width INTEGER,
    source_height INTEGER,
    source_scale REAL,
    crop_json TEXT,
    geometry_revision INTEGER NOT NULL DEFAULT 0,
    failure_reason TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    last_state_at TEXT NOT NULL,
    ended_at TEXT,
    UNIQUE(owner_device_id, client_request_id)
);

CREATE INDEX computer_sessions_conversation_idx
    ON computer_sessions(conversation_id, updated_at DESC);

CREATE INDEX computer_sessions_owner_idx
    ON computer_sessions(owner_device_id, updated_at DESC);
