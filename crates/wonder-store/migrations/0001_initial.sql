-- Wonder's canonical metadata boundary. Codex remains authoritative for full thread content.
CREATE TABLE IF NOT EXISTS devices (
    id TEXT PRIMARY KEY NOT NULL,
    label TEXT NOT NULL,
    role TEXT NOT NULL CHECK (role = 'owner'),
    public_key_jwk TEXT NOT NULL,
    created_at TEXT NOT NULL,
    last_seen_at TEXT,
    revoked_at TEXT
);

CREATE TABLE IF NOT EXISTS sessions (
    token_hash TEXT PRIMARY KEY NOT NULL,
    device_id TEXT NOT NULL REFERENCES devices(id),
    csrf_hash TEXT NOT NULL,
    expires_at_ms INTEGER NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS messages (
    id TEXT PRIMARY KEY NOT NULL,
    device_id TEXT NOT NULL REFERENCES devices(id),
    client_message_id TEXT NOT NULL,
    body_sha256 TEXT NOT NULL,
    conversation_id TEXT NOT NULL,
    state TEXT NOT NULL,
    codex_thread_id TEXT,
    codex_turn_id TEXT,
    created_at TEXT NOT NULL,
    UNIQUE (device_id, client_message_id)
);

CREATE TABLE IF NOT EXISTS events (
    event_id TEXT PRIMARY KEY NOT NULL,
    host_epoch TEXT NOT NULL,
    sequence INTEGER NOT NULL,
    occurred_at TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    UNIQUE (host_epoch, sequence)
);
