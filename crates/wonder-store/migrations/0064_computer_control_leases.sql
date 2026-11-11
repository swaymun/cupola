CREATE TABLE computer_control_leases (
    id TEXT PRIMARY KEY,
    client_request_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    owner_device_id TEXT NOT NULL,
    host_installation_id TEXT NOT NULL,
    conversation_id TEXT NOT NULL,
    session_generation INTEGER NOT NULL,
    source_id TEXT,
    geometry_revision INTEGER NOT NULL,
    status TEXT NOT NULL,
    last_sequence INTEGER NOT NULL DEFAULT 0,
    acquired_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    released_at TEXT,
    UNIQUE(owner_device_id, client_request_id),
    FOREIGN KEY(session_id) REFERENCES computer_sessions(id) ON DELETE CASCADE
);

CREATE UNIQUE INDEX computer_control_leases_active_host_idx
    ON computer_control_leases(host_installation_id)
    WHERE status = 'active';

CREATE INDEX computer_control_leases_session_idx
    ON computer_control_leases(session_id, updated_at DESC);
